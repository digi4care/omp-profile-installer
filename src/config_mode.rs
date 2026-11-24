//! Per-command config handling modes.
//!
//! Config modes are selected per command and are never remembered: the
//! installer derives update and remove behavior from what the manifest
//! recorded, not from flags a previous command happened to pass.

use std::fmt;
use std::str::FromStr;

/// Which side of a merge conflict wins when the profile already has a
/// value for a key the topology also ships.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergePrecedence {
    /// The existing profile value wins (default).
    Profile,
    /// The topology value wins.
    Topology,
}

impl MergePrecedence {
    pub fn as_str(&self) -> &'static str {
        match self {
            MergePrecedence::Profile => "profile",
            MergePrecedence::Topology => "topology",
        }
    }
}

impl fmt::Display for MergePrecedence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MergePrecedence {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "profile" => Ok(MergePrecedence::Profile),
            "topology" => Ok(MergePrecedence::Topology),
            other => Err(format!(
                "invalid config precedence '{other}'; expected 'profile' or 'topology'"
            )),
        }
    }
}

/// How a command handles the topology's `config.yml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigMode {
    /// Safe default: never rewrite an existing active `config.yml`;
    /// publish the topology config as a `config.yml.new` candidate.
    Candidate,
    /// Structural merge of the topology config into the active config.
    Merge {
        /// Which side wins when both define a value.
        precedence: MergePrecedence,
    },
    /// Full replacement of the active config (backup + restore on remove).
    Replace,
}
