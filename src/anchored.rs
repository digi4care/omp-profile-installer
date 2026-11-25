//! Descriptor-anchored filesystem operations (TOCTOU mitigation).
//!
//! Every mutation and read-verification the installer performs inside
//! `~/.omp`, the configured skills root, and the per-agent state tree is
//! issued relative to a pinned directory descriptor (`RootFd`) via the
//! `openat` family with `O_NOFOLLOW`, instead of against reconstructed
//! path strings. A path string can be swapped between the check and the
//! use; a directory descriptor cannot. This module is the single place
//! that layer lives.
//!
//! Trust model, mirroring the previous path-based implementation exactly:
//!
//! * A configured root (`agent`, `profiles`, custom `--agent-dir`, skills
//!   roots, state roots) is pinned by a strict component-by-component
//!   walk from a pinned `/` descriptor: EVERY component, ancestors
//!   included, is opened `O_NOFOLLOW`. A symlink anywhere on the path —
//!   not just at the root itself — fails closed with `ELOOP`, so an
//!   ancestor swapped for a symlink after path validation cannot redirect
//!   the configured root. Only two deliberate follows exist: `$HOME`
//!   acquisition resolves symlinks via `std::fs::canonicalize` and then
//!   re-pins the resolved path with the same strict walk (ambient
//!   environment, not an OMP-configured root), and the documented
//!   `~/.omp` root entry is followed once via
//!   `RootFd::open_subdir_follow`, whose link target is re-pinned with
//!   the same strict no-symlink policy.
//! * Operations descending further (`read_file`, `write_atomic`, ...)
//!   re-walk the remaining relative path component-by-component with
//!   `O_NOFOLLOW` and NEVER follow a symlink component, not even one
//!   whose target stays inside the pinned root: a raced parent swap fails
//!   the walk instead of redirecting the write. The approved
//!   main→profile topology links are resolved by `Roots` /
//!   `resolve_declared_at` / `ResolvedAt`, which re-pin the link target
//!   explicitly — never by generic walk following. The final component is
//!   never followed by any mutation (`O_NOFOLLOW` /
//!   `AT_SYMLINK_NOFOLLOW` / `renameat`, which replaces a swapped-in link
//!   instead of writing through it).
//! * Directory creation and replacement use atomic temp-file + rename
//!   sequences relative to the pinned parent descriptor, mirroring the
//!   permission and durability contracts of the `fsx` path-based helpers.
//!
//! Only Unix targets are supported (the release matrix is
//! `x86_64-unknown-linux-musl`, `x86_64-apple-darwin`, and
//! `aarch64-apple-darwin`); all primitives below bind `libc` directly.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use libc::{c_int, mode_t};

use crate::fsx;

/// Maximum readlink hops a single resolution follows before reporting
/// `ELOOP`. Deep chains are almost certainly loops; failing closed is
/// safe.
const MAX_SYMLINK_HOPS: usize = 40;

/// The kind of one directory entry, probed with `AT_SYMLINK_NOFOLLOW` so
/// a symlink is reported as a symlink, never as its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl EntryKind {
    fn from_mode(mode: mode_t) -> EntryKind {
        match mode & libc::S_IFMT {
            libc::S_IFREG => EntryKind::File,
            libc::S_IFDIR => EntryKind::Directory,
            libc::S_IFLNK => EntryKind::Symlink,
            _ => EntryKind::Other,
        }
    }
}

/// Why a bounded descent below a pinned root failed.
#[derive(Debug)]
pub enum SubdirError {
    /// A path component does not exist.
    NotFound,
    /// A symlink component's target escapes the pinned root's resolved
    /// path. Carries the offending absolutized target.
    Escapes(PathBuf),
    /// Any other I/O failure.
    Io(io::Error),
}

impl From<SubdirError> for io::Error {
    fn from(err: SubdirError) -> io::Error {
        match err {
            SubdirError::NotFound => io::Error::from_raw_os_error(libc::ENOENT),
            SubdirError::Escapes(target) => io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "symlink component resolves to '{}' outside the pinned root",
                    target.display()
                ),
            ),
            SubdirError::Io(err) => err,
        }
    }
}

impl From<io::Error> for SubdirError {
    fn from(err: io::Error) -> SubdirError {
        if err.kind() == io::ErrorKind::NotFound {
            SubdirError::NotFound
        } else {
            SubdirError::Io(err)
        }
    }
}

/// A pinned directory descriptor plus its fully resolved path.
///
/// The path is informational (messages, manifest recording); every actual
/// operation goes through the descriptor. The descriptor is
/// `Arc`-shared so `Clone` duplicates the pin without ever re-opening
/// the recorded path (a re-open could switch roots under a raced rename).
pub struct RootFd {
    fd: Arc<OwnedFd>,
    path: PathBuf,
}

/// Outcome of [`RootFd::open_strict_root`].
pub enum StrictRoot {
    /// The root is pinned (every component is a real directory).
    Pinned(RootFd),
    /// The root does not exist.
    Absent,
    /// A component of the path is a symlink: refused.
    Symlink,
}

impl Clone for RootFd {
    fn clone(&self) -> RootFd {
        RootFd {
            fd: Arc::clone(&self.fd),
            path: self.path.clone(),
        }
    }
}

impl RootFd {
    /// Pin `/`, the fixed starting point of every absolute acquisition
    /// walk.
    fn slash() -> io::Result<RootFd> {
        Ok(RootFd {
            fd: Arc::new(open_dir_fd_at(libc::AT_FDCWD, OsStr::new("/"))?),
            path: PathBuf::from("/"),
        })
    }

    /// Pin a directory path (absolute, or relative to the cwd) at its
    /// fully resolved location. This is the ONE lenient acquisition in
    /// the module, reserved for `$HOME` (ambient environment, not an
    /// OMP-configured root): `std::fs::canonicalize` resolves a symlink
    /// at any component, and the resolved path is then pinned with the
    /// same strict no-symlink walk as [`RootFd::open_strict`], so a
    /// component swapped for a symlink between resolution and pinning
    /// fails closed instead of being followed. [`RootFd::path`] is the
    /// resolved location. Every configured root uses
    /// [`RootFd::open_strict`] directly, which refuses a symlink at any
    /// component.
    pub fn open(path: &Path) -> io::Result<RootFd> {
        Self::open_strict(&std::fs::canonicalize(path)?)
    }

    /// Pin `path` as a configured root with a STRICT walk: EVERY
    /// component — ancestors included — must be a real directory opened
    /// `O_NOFOLLOW`; a symlink anywhere on the path is refused (`ELOOP`)
    /// instead of followed, so an ancestor swapped for a symlink after
    /// path validation cannot redirect the root, and pinning lands on the
    /// exact directories the caller validated. Used for custom
    /// `--agent-dir`, skills roots, and state roots. See
    /// [`RootFd::open_subdir_follow`] for the single documented
    /// exception (`~/.omp` itself).
    pub fn open_strict(path: &Path) -> io::Result<RootFd> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        RootFd::slash()?
            .descend(&split_components(&absolute), None)
            .map_err(SubdirError::into)
    }

    /// Strict-root open with the absent / symlinked outcomes named, so
    /// callers can refuse a symlinked configured root with their own
    /// message instead of a bare `io::Error`.
    pub fn open_strict_root(path: &Path) -> io::Result<StrictRoot> {
        match RootFd::open_strict(path) {
            Ok(pin) => Ok(StrictRoot::Pinned(pin)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(StrictRoot::Absent),
            Err(err) if err.raw_os_error() == Some(libc::ELOOP) => Ok(StrictRoot::Symlink),
            Err(err) => Err(err),
        }
    }

    /// Pin `path`, creating it (and missing parents, chmodded to `0o700`
    /// like `fsx::makedirs_private`) when absent. Existing components are
    /// walked with the same strict no-symlink policy as
    /// [`RootFd::open_strict`]: every component is opened `O_NOFOLLOW`
    /// from the pinned parent of the preceding component, and a symlink
    /// swapped in at any existing component fails closed instead of being
    /// followed. The final component is created by `mkdirat` and then
    /// opened `O_NOFOLLOW`, so a symlink swapped in concurrently is
    /// refused rather than followed.
    pub fn open_or_create_private(path: &Path) -> io::Result<RootFd> {
        Self::open_or_create_mode(path, 0o700, true)
    }

    /// Pin `path`, creating it (and missing parents with the process
    /// umask applied to `0o777`, like `std::fs::create_dir_all`) when
    /// absent. Creation is component-wise `mkdirat` from a pinned
    /// parent, not a path-based `create_dir_all`.
    pub fn open_or_create(path: &Path) -> io::Result<RootFd> {
        Self::open_or_create_mode(path, 0o777, false)
    }

    fn open_or_create_mode(path: &Path, mode: u32, chmod_created: bool) -> io::Result<RootFd> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        RootFd::slash()?
            .descend(
                &split_components(&absolute),
                Some((mode as mode_t, chmod_created)),
            )
            .map_err(SubdirError::into)
    }

    /// Descend to the directory that the single component `name`
    /// resolves to, following a symlink at that ONE component — the
    /// documented `~/.omp` root entry, the only place a symlink is ever
    /// honored. The link target is re-pinned with the same strict
    /// no-symlink policy as [`RootFd::open_strict`]: every component of
    /// the target must be a real directory. Everything below the pinned
    /// `.omp` directory is opened `O_NOFOLLOW` via [`RootFd::open_subdir`].
    pub fn open_subdir_follow(&self, name: &str) -> io::Result<RootFd> {
        match open_dir_fd_at(self.fd.as_raw_fd(), OsStr::new(name)) {
            Ok(fd) => Ok(RootFd {
                fd: Arc::new(fd),
                path: self.path.join(name),
            }),
            Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
                let raw = read_link_at(&self.fd, OsStr::new(name))?;
                let target = absolutize(&self.path, raw.as_os_str());
                RootFd::open_strict(&target)
            }
            Err(err) => Err(err),
        }
    }

    /// The pinned directory's fully resolved path (informational only).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `true` when both descriptors pin the same underlying directory
    /// (device + inode), regardless of the spelling used to reach it.
    pub fn same_dir(&self, other: &RootFd) -> io::Result<bool> {
        Ok(stat_dev_ino(&self.fd)? == stat_dev_ino(&other.fd)?)
    }

    /// Descend to the real directory at `rel` below this root. EVERY
    /// component must be a real directory opened `O_NOFOLLOW`; a symlink
    /// component at any position (including the final one) fails closed
    /// with `ELOOP` instead of being followed. The approved topology
    /// links are resolved by `Roots` / `resolve_declared_at` /
    /// `ResolvedAt`, never by generic walk following.
    pub fn open_subdir(&self, rel: &Path) -> Result<RootFd, SubdirError> {
        self.descend(&split_components(rel), None)
    }

    /// `true` when `rel` exists below this root (any entry kind).
    pub fn exists(&self, rel: &str) -> io::Result<bool> {
        Ok(self.kind(rel)?.is_some())
    }

    /// `true` when any component of `rel` is a symlink — the
    /// descriptor-anchored analogue of `fsx::has_symlink_component`: each
    /// component is classified `AT_SYMLINK_NOFOLLOW` from this pinned
    /// descriptor, ancestors re-descended `O_NOFOLLOW`. A missing path or
    /// a non-directory in the way stops the walk with `false`, exactly
    /// like the path-based helper.
    pub fn has_symlink_component(&self, rel: &str) -> io::Result<bool> {
        let parts = rel_components(rel)?;
        let mut current = self.clone();
        for (index, part) in parts.iter().enumerate() {
            if index == parts.len() - 1 {
                return Ok(matches!(
                    lstat_kind(&current.fd, part)?,
                    Some(EntryKind::Symlink)
                ));
            }
            match lstat_kind(&current.fd, part)? {
                Some(EntryKind::Symlink) => return Ok(true),
                Some(EntryKind::Directory) => {
                    current = current
                        .descend(std::slice::from_ref(part), None)
                        .map_err(io::Error::from)?;
                }
                // Missing path or a non-directory in the way: nothing
                // further can be a link.
                _ => return Ok(false),
            }
        }
        Ok(false)
    }

    /// The kind of `rel`, or `None` when it does not exist. Symlinks are
    /// reported as symlinks (never followed).
    pub fn kind(&self, rel: &str) -> io::Result<Option<EntryKind>> {
        match self.walk_parent(rel, None) {
            Ok((dir, name)) => lstat_kind(&dir.fd, &name),
            Err(SubdirError::NotFound) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Read the regular file at `rel`: `None` when absent, an error when
    /// the final component is a symlink (a swapped-in link is a
    /// concurrent-change signal, never something to read through).
    pub fn read_file(&self, rel: &str) -> io::Result<Option<Vec<u8>>> {
        let (dir, name) = match self.walk_parent(rel, None) {
            Ok(walked) => walked,
            Err(SubdirError::NotFound) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let name = c_string(&name)?;
        let fd = unsafe {
            libc::openat(
                dir.fd.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0,
            )
        };
        if fd < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(err);
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        Ok(Some(data))
    }

    /// The permission bits (`& 0o7777`) of `rel`, or `None` when absent.
    pub fn file_mode(&self, rel: &str) -> io::Result<Option<u32>> {
        match self.walk_parent(rel, None) {
            Ok((dir, name)) => Ok(lstat_at(&dir.fd, &name)?.map(|stat| stat.st_mode & 0o7777)),
            Err(SubdirError::NotFound) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Atomically replace the file at `rel` (temp file + `renameat`,
    /// mirroring `fsx::atomic_write`). Missing parent directories are
    /// created with the process umask applied to `0o777`, exactly like
    /// `fsx::atomic_write`'s `create_dir_all`. The mode is applied to
    /// the written file when given (otherwise the restrictive `0o600`
    /// default of `fsx::atomic_write` applies).
    pub fn write_atomic(&self, rel: &str, data: &[u8], mode: Option<u32>) -> io::Result<()> {
        let (dir, name) = self.walk_parent(rel, Some((0o777, false)))?;
        write_via_temp(&dir.fd, &name, data, mode)
    }

    /// Owner-only variant of [`RootFd::write_atomic`]: missing parents
    /// are created and chmodded to `0o700` (mirroring
    /// `fsx::write_private`), the file itself is written `0o600`.
    pub fn write_private(&self, rel: &str, data: &[u8]) -> io::Result<()> {
        let (dir, name) = self.walk_parent(rel, Some((0o700, true)))?;
        write_via_temp(&dir.fd, &name, data, Some(0o600))
    }

    /// Atomic no-replace create at `rel`, mirroring
    /// `fsx::atomic_create_exclusive`: the bytes are written to an
    /// owner-only temp file, synced, optionally chmodded, and published
    /// with `linkat` (a hard link, which cannot replace an existing
    /// name), so the final name never appears partially written. The
    /// temp name is always cleaned up; the published file holds the data
    /// independent of it. `Ok(true)` when the bytes were published,
    /// `Ok(false)` when a concurrent file, directory, or symlink already
    /// occupies the path — the existing entry is never followed or
    /// replaced.
    pub fn write_exclusive(&self, rel: &str, data: &[u8], mode: Option<u32>) -> io::Result<bool> {
        // Parent creation mirrors `fsx::atomic_create_exclusive`'s
        // `makedirs_private`: `0o700`, chmodded to defeat the umask.
        let (dir, name) = self.walk_parent(rel, Some((0o700, true)))?;
        let temp = write_temp(&dir.fd, &name, data, mode)?;
        let temp_c = c_string(&temp)?;
        let name_c = c_string(&name)?;
        let rc = unsafe {
            libc::linkat(
                dir.fd.as_raw_fd(),
                temp_c.as_ptr(),
                dir.fd.as_raw_fd(),
                name_c.as_ptr(),
                0,
            )
        };
        let result = if rc == 0 {
            Ok(true)
        } else {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EEXIST) {
                Ok(false)
            } else {
                Err(err)
            }
        };
        // The temp name is ours whether or not publication succeeded; a
        // published path holds the data independent of it.
        unsafe {
            libc::unlinkat(dir.fd.as_raw_fd(), temp_c.as_ptr(), 0);
        }
        result
    }

    /// Remove the file at `rel` (never follows the final component).
    pub fn remove_file(&self, rel: &str) -> io::Result<()> {
        let (dir, name) = self.walk_parent(rel, None)?;
        let name = c_string(&name)?;
        let rc = unsafe { libc::unlinkat(dir.fd.as_raw_fd(), name.as_ptr(), 0) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Remove the empty directory at `rel` (never follows the final
    /// component), propagating the `unlinkat` result: `Ok(())` only when
    /// the directory was actually removed, so a caller racing an
    /// attacker who swaps a parent for a symlink — or refills the
    /// directory before the `rmdir` — sees the failed removal instead of
    /// silently continuing.
    pub fn remove_empty_dir(&self, rel: &str) -> io::Result<()> {
        let (dir, name) = self.walk_parent(rel, None)?;
        let name = c_string(&name)?;
        let rc = unsafe { libc::unlinkat(dir.fd.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Best-effort removal of the empty directory at `rel`. The removal
    /// result is returned for callers that record it (install's rollback
    /// tracks whether the `rmdir` was attempted), but every failure mode
    /// (non-empty, missing, raced away) stays best-effort: the `unlinkat`
    /// outcome is deliberately not propagated, mirroring remove's
    /// `remove_empty_dir`.
    pub fn try_remove_empty_dir(&self, rel: &str) -> io::Result<()> {
        let (dir, name) = self.walk_parent(rel, None)?;
        let name = c_string(&name)?;
        unsafe {
            libc::unlinkat(dir.fd.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
        }
        Ok(())
    }

    /// Create the directory chain at `rel` (each created directory
    /// chmodded to `mode` when `chmod_created`, mirroring
    /// `fsx::makedirs_private`).
    pub fn makedirs(&self, rel: &str, mode: mode_t, chmod_created: bool) -> io::Result<()> {
        self.walk_parent(rel, Some((mode, chmod_created)))
            .map_err(io::Error::from)
            .map(|_| ())
    }

    /// Recursively remove the directory tree at `rel`, descriptor-
    /// relative at every level (sub-directories are re-pinned with
    /// `O_NOFOLLOW` before descending, entries are unlinked via
    /// `unlinkat`, and a symlink is always unlinked itself, never
    /// followed). Used only on installer-created snapshot trees; a
    /// missing `ENOENT` is ignored like `remove_dir_all`.
    pub fn remove_tree(&self, rel: &str) -> io::Result<()> {
        let (dir, name) = match self.walk_parent(rel, None) {
            Ok(walked) => walked,
            Err(SubdirError::NotFound) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        match lstat_kind(&dir.fd, &name)? {
            None => return Ok(()),
            Some(EntryKind::Directory) => {}
            Some(_) => return Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
        }
        let fd = open_dir_fd_at(dir.fd.as_raw_fd(), &name)?;
        remove_tree_fd(&fd)?;
        let name = c_string(&name)?;
        let rc = unsafe { libc::unlinkat(dir.fd.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::NotFound {
                return Err(err);
            }
        }
        Ok(())
    }

    /// List the entries of the directory at `rel` as `(name, kind)` pairs
    /// with kinds probed `AT_SYMLINK_NOFOLLOW` (a symlinked entry is
    /// reported as `Symlink`, never followed) — the descriptor analogue
    /// of `read_dir` + `DirEntry::file_type`.
    pub fn list(&self, rel: &str) -> io::Result<Vec<(OsString, EntryKind)>> {
        let dir = if rel.is_empty() {
            self.clone()
        } else {
            self.open_subdir(Path::new(rel)).map_err(io::Error::from)?
        };
        read_entries(&dir.fd)
    }

    /// Resolve `rel` to its canonical path below this root: every
    /// component is walked `O_NOFOLLOW`, and a symlink anywhere on the
    /// path (including the final component) is followed by restarting
    /// the walk on its target, which must stay inside this root. The
    /// result is informational only — nothing is opened at it.
    pub fn canonical_entry_path(&self, rel: &str) -> Result<PathBuf, SubdirError> {
        let parts = rel_components(rel)?;
        let (last, parents) = parts.split_last().expect("rel_components is non-empty");
        let mut dir = self.descend(parents, None)?;
        let mut name = last.clone();
        let mut hops = 0usize;
        loop {
            match lstat_kind(&dir.fd, &name)? {
                None => return Err(SubdirError::NotFound),
                Some(EntryKind::Symlink) => {
                    hops += 1;
                    if hops > MAX_SYMLINK_HOPS {
                        return Err(SubdirError::Io(io::Error::from_raw_os_error(libc::ELOOP)));
                    }
                    let raw = read_link_at(&dir.fd, &name).map_err(SubdirError::Io)?;
                    let target = absolutize(&dir.path, raw.as_os_str());
                    if !target.starts_with(self.path()) {
                        return Err(SubdirError::Escapes(target));
                    }
                    // Restart on the link target, which is lexically
                    // inside this root, descending the remaining
                    // components from the pinned root itself.
                    let target_rel = target
                        .strip_prefix(self.path())
                        .expect("escape checked above");
                    let target_parts = split_components(target_rel);
                    let (target_name, target_parents) = target_parts
                        .split_last()
                        .expect("a non-root target has a final component");
                    dir = self.descend(target_parents, None)?;
                    name = target_name.clone();
                }
                Some(_) => return Ok(dir.path.join(name)),
            }
        }
    }

    /// Read the symlink at `rel` without following it.
    pub fn read_link(&self, rel: &str) -> io::Result<OsString> {
        let (dir, name) = self.walk_parent(rel, None)?;
        read_link_at(&dir.fd, &name)
    }

    /// Descend to the parent of `rel`, returning the pinned parent
    /// descriptor and the final component name (the final component
    /// itself is untouched: callers open, unlink, or rename it with
    /// their own `O_NOFOLLOW` semantics). The parent walk is strict:
    /// every parent component is opened `O_NOFOLLOW`, so a raced parent
    /// swap fails the walk instead of redirecting the operation.
    fn walk_parent(
        &self,
        rel: &str,
        create: Option<(mode_t, bool)>,
    ) -> Result<(RootFd, OsString), SubdirError> {
        let parts = rel_components(rel)?;
        let (last, parents) = parts.split_last().expect("rel_components is non-empty");
        let parent = self.descend(parents, create)?;
        Ok((parent, last.clone()))
    }

    /// Core strict descent: every component of `parts` must open as a
    /// real directory (`O_NOFOLLOW`) below the pinned parent of the
    /// preceding component; a symlink at ANY component (ancestors
    /// included) fails closed with `ELOOP` instead of being followed, so
    /// a parent swapped for a link between validation and use fails the
    /// walk instead of redirecting it. With `create`, missing components
    /// are created with `mkdirat`; when `chmod_created`, the new
    /// directory is chmodded to `mode` through the descriptor pinned
    /// `O_NOFOLLOW` on the name WE just created — a path-based
    /// `fchmodat(..., 0)` could be redirected by a name swapped for a
    /// symlink between `mkdirat` and the chmod, chmodding outside the
    /// pinned tree.
    fn descend(
        &self,
        parts: &[OsString],
        create: Option<(mode_t, bool)>,
    ) -> Result<RootFd, SubdirError> {
        let mut current = self.clone();
        for name in parts {
            if name.as_bytes() == b"." || name.as_bytes() == b".." {
                return Err(SubdirError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsafe path component '{}'", name.to_string_lossy()),
                )));
            }
            match open_dir_fd_at(current.fd.as_raw_fd(), name) {
                Ok(next) => {
                    current.path.push(name);
                    current.fd = Arc::new(next);
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    let Some((mode, chmod_created)) = create else {
                        return Err(SubdirError::NotFound);
                    };
                    let name_c = c_string(name).map_err(SubdirError::Io)?;
                    let rc =
                        unsafe { libc::mkdirat(current.fd.as_raw_fd(), name_c.as_ptr(), mode) };
                    let created = if rc == 0 {
                        let fd = open_dir_fd_at(current.fd.as_raw_fd(), name)?;
                        if chmod_created {
                            // The exact mode matters (private roots
                            // defeat the umask), so a chmod failure must
                            // abort the walk instead of leaving a lax
                            // directory behind.
                            let frc = unsafe { libc::fchmod(fd.as_raw_fd(), mode) };
                            if frc != 0 {
                                return Err(SubdirError::Io(io::Error::last_os_error()));
                            }
                        }
                        fd
                    } else {
                        let err = io::Error::last_os_error();
                        if err.raw_os_error() != Some(libc::EEXIST) {
                            return Err(SubdirError::Io(err));
                        }
                        // Lost a creation race: pin the winner (opened
                        // `O_NOFOLLOW`, so a swapped-in symlink is still
                        // refused), unchmodded like an existing directory.
                        open_dir_fd_at(current.fd.as_raw_fd(), name)?
                    };
                    current.path.push(name);
                    current.fd = Arc::new(created);
                }
                Err(err) => return Err(SubdirError::from(err)),
            }
        }
        Ok(current)
    }
}

/// Split a relative path into its validated components. Rejects empty,
/// `.`, and `..` components: manifest paths are `fsx::safe_rel`-checked
/// before they ever reach this layer, so anything else is a bug.
fn rel_components(rel: &str) -> io::Result<Vec<OsString>> {
    let parts: Vec<OsString> = rel
        .as_bytes()
        .split(|byte| *byte == b'/')
        .map(|part| OsStr::from_bytes(part).to_os_string())
        .collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| part.is_empty() || part.as_bytes() == b"." || part.as_bytes() == b"..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe relative path '{rel}'"),
        ));
    }
    Ok(parts)
}

/// Split `path` into its non-empty components for [`RootFd::descend`];
/// `.` and `..` components are rejected by the walk itself.
fn split_components(path: &Path) -> Vec<OsString> {
    path.as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty())
        .map(|part| OsStr::from_bytes(part).to_os_string())
        .collect()
}

fn c_string(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL byte in path"))
}

/// Open `name` (a single path component) below `dirfd` as a real
/// directory: `O_NOFOLLOW` refuses a swapped-in symlink with `ELOOP`.
fn open_dir_fd_at(dirfd: c_int, name: &OsStr) -> io::Result<OwnedFd> {
    let name = c_string(name)?;
    let fd = unsafe {
        libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `fstatat(AT_SYMLINK_NOFOLLOW)` of one entry; `None` when absent.
fn lstat_at(dirfd: &OwnedFd, name: &OsStr) -> io::Result<Option<libc::stat>> {
    let name = c_string(name)?;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            dirfd.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(err);
    }
    Ok(Some(stat))
}

/// The [`EntryKind`] of one entry; `None` when absent.
fn lstat_kind(dirfd: &OwnedFd, name: &OsStr) -> io::Result<Option<EntryKind>> {
    Ok(lstat_at(dirfd, name)?.map(|stat| EntryKind::from_mode(stat.st_mode)))
}

fn read_link_at(dirfd: &OwnedFd, name: &OsStr) -> io::Result<OsString> {
    let name = c_string(name)?;
    // PATH_MAX on every supported target; symlink targets are bounded by
    // it at creation, so a full buffer means truncation and is reported
    // as an error rather than silently misresolved.
    let mut buffer = [0u8; 4096];
    let read = unsafe {
        libc::readlinkat(
            dirfd.as_raw_fd(),
            name.as_ptr(),
            buffer.as_mut_ptr() as *mut libc::c_char,
            buffer.len(),
        )
    };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    if read as usize == buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "symlink target too long",
        ));
    }
    Ok(OsStr::from_bytes(&buffer[..read as usize]).to_os_string())
}

fn stat_dev_ino(fd: &OwnedFd) -> io::Result<(libc::dev_t, libc::ino_t)> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((stat.st_dev, stat.st_ino))
}

/// Write `data` to a fresh owner-only temp file beside `dirfd`, apply
/// `mode` when given, and fsync it for durability, returning the temp
/// name. On any failure the temp is unlinked.
fn write_temp(
    dirfd: &OwnedFd,
    name: &OsStr,
    data: &[u8],
    mode: Option<u32>,
) -> io::Result<OsString> {
    let temp = fsx::temp_name(name);
    let temp_c = c_string(&temp)?;
    let written = (|| -> io::Result<()> {
        let fd = unsafe {
            libc::openat(
                dirfd.as_raw_fd(),
                temp_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(data)?;
        if let Some(mode) = mode {
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = written {
        // Best-effort temp cleanup: never leak the owner-only scratch
        // file when the write failed.
        unsafe {
            libc::unlinkat(dirfd.as_raw_fd(), temp_c.as_ptr(), 0);
        }
        return Err(err);
    }
    Ok(temp)
}

/// Atomic temp-file + `renameat` write below a pinned parent descriptor,
/// mirroring `fsx::atomic_write`. The rename replaces a swapped-in
/// final-component symlink instead of following it, exactly like the
/// path-based rename it replaces.
fn write_via_temp(dirfd: &OwnedFd, name: &OsStr, data: &[u8], mode: Option<u32>) -> io::Result<()> {
    let temp = write_temp(dirfd, name, data, mode)?;
    let temp_c = c_string(&temp)?;
    let name_c = c_string(name)?;
    let rc = unsafe {
        libc::renameat(
            dirfd.as_raw_fd(),
            temp_c.as_ptr(),
            dirfd.as_raw_fd(),
            name_c.as_ptr(),
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        // Best-effort temp cleanup: never leak the owner-only scratch
        // file when the rename failed.
        unsafe {
            libc::unlinkat(dirfd.as_raw_fd(), temp_c.as_ptr(), 0);
        }
        return Err(err);
    }
    Ok(())
}

/// `(name, kind)` of every entry of the pinned directory `fd` (`.` and
/// `..` excluded), kinds probed `AT_SYMLINK_NOFOLLOW` so a symlink is
/// reported as a symlink, never its target; entries that vanish
/// mid-listing are skipped.
fn read_entries(fd: &OwnedFd) -> io::Result<Vec<(OsString, EntryKind)>> {
    let mut entries = Vec::new();
    let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    let dirp = unsafe { libc::fdopendir(dup) };
    if dirp.is_null() {
        return Err(io::Error::last_os_error());
    }
    loop {
        let entry = unsafe { libc::readdir(dirp) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let name = OsStr::from_bytes(name).to_os_string();
        // Vanished mid-listing: report nothing for it.
        if let Ok(Some(kind)) = lstat_kind(fd, &name) {
            entries.push((name, kind));
        }
    }
    unsafe {
        libc::closedir(dirp);
    }
    Ok(entries)
}

/// Recursively unlink every entry of the pinned directory `fd`; the
/// caller removes the (now empty) directory itself. Sub-directories are
/// re-pinned with `O_NOFOLLOW` before descending; symlinks are unlinked
/// themselves; vanished entries are ignored (`remove_dir_all` parity).
fn remove_tree_fd(fd: &OwnedFd) -> io::Result<()> {
    for (name, kind) in read_entries(fd)? {
        if kind == EntryKind::Directory {
            let sub = open_dir_fd_at(fd.as_raw_fd(), &name)?;
            remove_tree_fd(&sub)?;
        }
        let name_c = c_string(&name)?;
        let flags = if kind == EntryKind::Directory {
            libc::AT_REMOVEDIR
        } else {
            0
        };
        let rc = unsafe { libc::unlinkat(fd.as_raw_fd(), name_c.as_ptr(), flags) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::NotFound {
                return Err(err);
            }
        }
    }
    Ok(())
}

/// Absolutize `raw` against `base` (when `raw` is relative) and
/// lexically resolve `.`/`..` — the same lexical normalization the
/// path-based implementation used (`fsx::resolve_loose` /
/// `symlinks::normalize_dotdot`).
pub fn absolutize(base: &Path, raw: &OsStr) -> PathBuf {
    let joined = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        base.join(raw)
    };
    let mut parts: Vec<OsString> = Vec::new();
    for component in joined.components() {
        match component {
            Component::RootDir => parts.clear(),
            Component::Normal(segment) => parts.push(segment.to_os_string()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir => {}
            Component::Prefix(prefix) => parts.push(prefix.as_os_str().to_os_string()),
        }
    }
    let mut result = PathBuf::from("/");
    for part in parts {
        result.push(part);
    }
    result
}

#[cfg(test)]
mod tests;
