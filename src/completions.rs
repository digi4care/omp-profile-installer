//! Runtime-generated shell completions with filtering for CLI relationships.
//!
//! The shell adapters pass the current command line to the hidden
//! `__complete` subcommand. Clap supplies option/value candidates; this module
//! removes candidates that would violate relationships enforced by the CLI.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use clap_complete::engine::{self, CompletionCandidate};

use crate::{Cli, profiles, registry, state_anchor};

pub(crate) fn script(shell: &str) -> Option<&'static str> {
    match shell {
        "bash" => Some(
            r#"_omp_topology_complete() {
    COMPREPLY=( $(command omp-topology __complete -- "${COMP_WORDS[@]:1}") )
}
complete -o nosort -F _omp_topology_complete omp-topology
"#,
        ),
        "fish" => Some(
            r#"function __omp_topology_complete
    set -l words (commandline -opc)
    set -l current (commandline -ct)
    if test (count $current) -eq 0
        set current ''
    end
    command omp-topology __complete -- $words "$current"
end
complete -c omp-topology -f -a '(__omp_topology_complete)'
"#,
        ),
        "zsh" => Some(
            r#"#compdef omp-topology

local -a candidates completion_words
completion_words=("${(@)words[2,CURRENT]}")
candidates=("${(@f)$(command omp-topology __complete -- "${completion_words[@]}")}")
compadd -Q -- "${candidates[@]}"
"#,
        ),
        _ => None,
    }
}

pub(crate) fn complete_install_ids(current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok(anchor) = state_anchor() else {
        return Vec::new();
    };
    let Ok(registry) = registry::load_registry(&anchor) else {
        return Vec::new();
    };
    let prefix = current.to_string_lossy();
    registry
        .aliases
        .keys()
        .filter(|id| id.starts_with(prefix.as_ref()))
        .map(|id| CompletionCandidate::new(id.clone()))
        .collect()
}

pub(crate) fn complete_profiles(current: &OsStr) -> Vec<CompletionCandidate> {
    let Ok(anchor) = state_anchor() else {
        return Vec::new();
    };
    let Ok(profiles) = profiles::discover_profiles(&anchor) else {
        return Vec::new();
    };
    let prefix = current.to_string_lossy();
    profiles
        .into_iter()
        .filter_map(|profile| profile.name)
        .filter(|name| name.starts_with(prefix.as_ref()))
        .map(CompletionCandidate::new)
        .collect()
}

pub(crate) fn complete(words: Vec<OsString>) {
    let mut words = normalize_words(words);
    if words.len() == 1 {
        words.push(OsString::new());
    }
    let current_index = words.len() - 1;
    let mut command = <Cli as clap::CommandFactory>::command();
    let current_dir = std::env::current_dir().ok();
    let candidates = engine::complete(
        &mut command,
        words.clone(),
        current_index,
        current_dir.as_deref(),
    )
    .unwrap_or_default();
    for candidate in candidates {
        let value = candidate.get_value().to_string_lossy();
        if !valid_candidate(&value, &words, current_index) {
            continue;
        }
        println!("{value}");
    }
}

fn normalize_words(mut words: Vec<OsString>) -> Vec<OsString> {
    let command_is_present = words
        .first()
        .is_some_and(|word| Path::new(word).file_name() == Some(OsStr::new("omp-topology")));
    if !command_is_present {
        words.insert(0, OsString::from("omp-topology"));
    }
    words
}

fn valid_candidate(candidate: &str, words: &[OsString], current_index: usize) -> bool {
    let prior = words.get(1..current_index).unwrap_or_default();
    let command = prior
        .iter()
        .filter_map(|word| word.to_str())
        .find(|word| !word.starts_with('-'))
        .unwrap_or_default();
    let has = |option: &str| option_present(prior, option);
    let is = |option: &str| candidate == option;

    if has(candidate) {
        return false;
    }

    match command {
        "doctor" | "status" | "remove" => {
            if has("--install-id") && (is("--profile") || is("--agent-dir"))
                || has("--profile") && (is("--install-id") || is("--agent-dir"))
                || has("--agent-dir") && (is("--install-id") || is("--profile"))
            {
                return false;
            }
        }
        "install" => {
            if has("--agent-dir") && (is("--profile") || is("--alias"))
                || has("--profile") && is("--agent-dir")
                || has("--alias") && is("--agent-dir")
                || is("--alias") && !has("--profile")
                || is("--ref") && !has("--source")
                || has("--config-merge") && is("--config-replace")
                || has("--config-replace") && (is("--config-merge") || is("--config-precedence"))
                || is("--config-precedence") && !has("--config-merge")
            {
                return false;
            }
        }
        "list" => {
            if is("--ref") && !has("--source") {
                return false;
            }
        }
        "update"
            if is("--ref") && !update_ref_is_github_source(prior)
                || has("--config-merge") && is("--config-replace")
                || has("--config-replace") && (is("--config-merge") || is("--config-precedence"))
                || is("--config-precedence") && !has("--config-merge")
            => {
                return false;
            }
        _ => {}
    }
    true
}

fn update_ref_is_github_source(words: &[OsString]) -> bool {
    if option_present(words, "--source") {
        return option_value(words, "--source").is_some_and(|spec| {
            matches!(
                crate::source::TopologySource::parse(spec),
                Ok(crate::source::TopologySource::GitHub { .. })
            )
        });
    }

    let Some(install_id) = option_value(words, "--install-id") else {
        return false;
    };
    let Ok(anchor) = state_anchor() else {
        return false;
    };
    let Ok(registry) = registry::load_registry(&anchor) else {
        return false;
    };
    matches!(
        registry.aliases.get(install_id).map(|entry| &entry.source),
        Some(registry::SourceSpec::GitHub { .. })
    )
}

fn option_value<'a>(words: &'a [OsString], option: &str) -> Option<&'a str> {
    for (index, word) in words.iter().enumerate() {
        let Some(word) = word.to_str() else {
            continue;
        };
        if word == option {
            return words.get(index + 1)?.to_str();
        }
        if let Some(value) = word
            .strip_prefix(option)
            .and_then(|suffix| suffix.strip_prefix('='))
        {
            return Some(value);
        }
    }
    None
}

fn option_present(words: &[OsString], option: &str) -> bool {
    words.iter().any(|word| {
        let word = word.to_string_lossy();
        word == option
            || word
                .strip_prefix(option)
                .is_some_and(|suffix| suffix.starts_with('='))
    })
}
