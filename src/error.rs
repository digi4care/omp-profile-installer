//! User-facing, typed errors for the installer.
//!
//! Every expected failure (bad input, unreadable state, colliding files)
//! is a `CliError`; `main` prints it as `error: <message>` on stderr and
//! exits with code 1. Unexpected panics are not part of the contract.

use std::fmt;
use std::path::PathBuf;

/// Expected, user-facing failure.
#[derive(Debug)]
pub enum CliError {
    /// Invalid user input: names, paths, or refused mutations.
    Usage(String),
    /// Topology metadata or overlay problems.
    Topology(String),
    /// YAML config problems: malformed documents, wrong top-level shape.
    Config(String),
    /// Ownership manifest problems.
    Manifest(String),
    /// Managed profile registry problems.
    Registry(String),
    /// A refused install whose file-name collision diagnostics name the
    /// aliases recorded as owning the colliding files. Renders exactly
    /// like [`CliError::Usage`]; `owners` lets recovery guidance name an
    /// actual owner instead of inferring one from the registry.
    InstallCollision {
        /// The rendered collision diagnostic (identical to `Usage`).
        message: String,
        /// Aliases named as owners by the collision diagnostics, in
        /// first-seen order.
        owners: Vec<String>,
    },
    /// Filesystem failure with the path that caused it.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Usage(message)
            | CliError::Topology(message)
            | CliError::Config(message)
            | CliError::Manifest(message)
            | CliError::Registry(message)
            | CliError::InstallCollision { message, .. } => f.write_str(message),
            CliError::Io { path, source } => {
                write!(f, "{}: {}", path.display(), source)
            }
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CliError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Wrap an I/O error with the path being operated on.
pub fn io_err(path: &std::path::Path, source: std::io::Error) -> CliError {
    CliError::Io {
        path: path.to_path_buf(),
        source,
    }
}
