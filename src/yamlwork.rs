//! Lossless YAML config merge and restore on top of `yaml-edit`.
//!
//! The parsed `Document` keeps the user's original bytes (comments,
//! blank lines, key order, scalar spellings). Edits splice CST nodes:
//!
//! * Installing a *new* key grafts the topology's node subtree, keeping the
//!   topology's own comments and style.
//! * Overwriting an *existing* leaf rebuilds the value from a comment-free
//!   `YamlValue`, so the user's entry comments survive on their own line.
//! * Removing restores prior values or deletes keys the topology added,
//!   then prunes mappings that became empty, all before any byte is written
//!   back. The caller compares the re-serialized document against the live
//!   file and skips the write when nothing changed.
//!
//! Topology documents are run through [`yaml_edit::migrate::to_yaml_1_2`]
//! before use so YAML 1.1 spellings in topology fixtures (e.g. `on`) keep
//! their historical boolean meaning while injected values use unambiguous
//! 1.2 spellings (`true`).
//!
//! Complexity: the merge walks the topology config once, O(T) for T the
//! topology config size, and each edit is a targeted CST splice, O(depth of
//! the edited path). There is no re-serialization of untouched content and
//! no repeated scanning of the user document.

use std::collections::BTreeMap;
use std::str::FromStr;

use yaml_edit::{Document, Mapping, ScalarType, ScalarValue, YamlNode, YamlValue};

use crate::error::CliError;
use crate::value::{self, Value};

/// Name of the agent config file in both the topology overlay and the
/// target agent directory.
pub const CONFIG_NAME: &str = "config.yml";

/// Parse exactly one YAML document that must be a mapping (or empty/null,
/// which normalizes to an empty mapping in the semantic model).
pub fn parse_config(bytes: &[u8], what: &str) -> Result<Document, CliError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| CliError::Config(format!("{what}: malformed YAML: invalid UTF-8")))?;
    let doc = Document::from_str(text)
        .map_err(|err| CliError::Config(format!("{what}: malformed YAML: {err}")))?;
    if doc.as_mapping().is_none() {
        let null_root = doc
            .as_scalar()
            .map(|scalar| scalar.is_null())
            .unwrap_or(false);
        let no_root =
            doc.as_scalar().is_none() && doc.as_sequence().is_none() && doc.as_tagged().is_none();
        if !null_root && !no_root {
            return Err(CliError::Config(format!(
                "{what} must be a YAML mapping at the top level"
            )));
        }
    }
    Ok(doc)
}

/// Rewrite YAML 1.1 constructs (e.g. `on`) to their YAML 1.2 spellings in
/// place, preserving the historical boolean meaning of old fixtures.
pub fn migrate(doc: &Document) {
    let _rewrites = yaml_edit::migrate::to_yaml_1_2(doc);
}

/// Build the type-strict semantic model of a document.
///
/// A null or empty document models as an empty mapping, like the previous
/// installer's decoding did.
pub fn model_of(doc: &Document, what: &str) -> Result<Value, CliError> {
    let yaml_value =
        YamlValue::from_document(doc).map_err(|err| CliError::Config(format!("{what}: {err}")))?;
    let model = value::from_yaml(&yaml_value)?;
    Ok(if model == Value::Null {
        Value::Map(BTreeMap::new())
    } else {
        model
    })
}

/// Convenience: migrate a clone of `doc`, then model it.
pub fn migrated_model(doc: &Document, what: &str) -> Result<Value, CliError> {
    let copy = doc.clone();
    migrate(&copy);
    model_of(&copy, what)
}

fn key_text(node: &YamlNode) -> Result<String, CliError> {
    node.as_scalar()
        .map(|scalar| ScalarValue::from_scalar(scalar).value().to_string())
        .ok_or_else(|| {
            CliError::Topology(
                "topology config contains a non-scalar mapping key, which \
                 the installer cannot merge"
                    .to_string(),
            )
        })
}

/// Refuse topology subtrees whose mapping keys are not plain strings.
///
/// The semantic model flattens every scalar key to its text, so a numeric
/// key `1` and a string key `'1'` compare equal there, but the concrete
/// document keeps them distinct. Addressing such a key through
/// `Mapping::set(&str, ..)` would re-type it or create a colliding
/// duplicate next to the user's original entry, so the merge fails closed
/// before any edit reaches the document.
fn check_mapping_keys(mapping: &Mapping) -> Result<(), CliError> {
    for (key_node, value_node) in mapping.iter() {
        let is_string = key_node
            .as_scalar()
            .map(|scalar| ScalarValue::from_scalar(scalar).scalar_type())
            == Some(ScalarType::String);
        if !is_string {
            return Err(CliError::Topology(
                "topology config contains a non-string mapping key, \
                 which the installer cannot merge without changing \
                 its type"
                    .to_string(),
            ));
        }
        check_string_keys(&value_node)?;
    }
    Ok(())
}

fn check_string_keys(node: &YamlNode) -> Result<(), CliError> {
    let mapping = node
        .as_mapping()
        .cloned()
        .or_else(|| node.as_tagged()?.as_mapping());
    if let Some(mapping) = mapping {
        return check_mapping_keys(&mapping);
    }
    if let Some(sequence) = node.as_sequence() {
        for index in 0..sequence.len() {
            let item = sequence.get(index).ok_or_else(|| {
                CliError::Topology("internal error: sequence item vanished".to_string())
            })?;
            check_string_keys(&item)?;
        }
    }
    Ok(())
}

/// Rebuild a node as a comment-free `YamlValue`, keeping scalar styles.
/// Used when overwriting an existing user value: the user's entry comments
/// stay on their line, the new value contributes none.
fn plain_value(node: &YamlNode) -> Option<YamlValue> {
    if let Some(scalar) = node.as_scalar() {
        return Some(YamlValue::Scalar(ScalarValue::from_scalar(scalar)));
    }
    if let Some(mapping) = node.as_mapping() {
        let mut map = BTreeMap::new();
        for (key_node, value_node) in mapping.iter() {
            let key = key_text(&key_node).ok()?;
            map.insert(key, plain_value(&value_node)?);
        }
        return Some(YamlValue::Mapping(map));
    }
    if let Some(sequence) = node.as_sequence() {
        let mut items = Vec::with_capacity(sequence.len());
        for index in 0..sequence.len() {
            items.push(plain_value(&sequence.get(index)?)?);
        }
        return Some(YamlValue::Sequence(items));
    }
    None
}

/// Set `value` for `key` on the user document at the current level.
///
/// `user_map` is `None` only for the document root before any key exists;
/// `Document::set` then creates the root mapping. A `None` map below the
/// root means the semantic model and the concrete syntax diverged (for
/// example numeric-looking keys resolve strictly), which the installer
/// refuses rather than risk clobbering user content.
fn set_value(
    user: &Document,
    user_map: Option<&Mapping>,
    at_root: bool,
    key: &str,
    value: impl yaml_edit::AsYaml,
) -> Result<(), CliError> {
    match user_map {
        Some(mapping) => {
            mapping.set(key, value);
            Ok(())
        }
        None if at_root => {
            if user.set(key, value) {
                Ok(())
            } else {
                Err(CliError::Config(format!(
                    "'{CONFIG_NAME}' has no mapping to merge into; replace it \
                     with a YAML mapping or an empty file"
                )))
            }
        }
        None => Err(CliError::Config(format!(
            "'{CONFIG_NAME}' key '{key}' could not be matched exactly in the \
             live file; refusing to guess"
        ))),
    }
}

/// Merge the topology config over the user config document, in place.
///
/// `topo` must already be YAML 1.2-migrated; its nodes are grafted into
/// `user`. `user_model` is the semantic model of the user config. Returns
/// the re-serialized document; the caller compares it with the original
/// bytes to detect a no-op merge.
pub fn merge_config(
    topo: &Document,
    user: &Document,
    user_model: &Value,
) -> Result<String, CliError> {
    if let Some(root) = topo.as_mapping() {
        // Fail closed on non-string topology keys before the first splice
        // so a refusal never leaves a half-merged document behind.
        check_mapping_keys(&root)?;
        if !root.is_empty()
            && user.as_mapping().is_none()
            && user.as_scalar().is_some_and(|scalar| scalar.is_null())
        {
            // An explicit `null` root models as an empty mapping, but the
            // concrete root is a scalar that `Document::set` cannot splice
            // into, and yaml-edit exposes no way to replace the root. The
            // null carries no config content, so merge into a fresh
            // document instead (any comments attached to the null line
            // are dropped).
            let fresh = Document::new_mapping();
            merge_level(
                &root,
                &fresh,
                None,
                true,
                &Value::Map(BTreeMap::new()),
                &mut Vec::new(),
            )?;
            return Ok(fresh.to_string());
        }
        merge_level(
            &root,
            user,
            user.as_mapping(),
            true,
            user_model,
            &mut Vec::new(),
        )?;
    }
    Ok(user.to_string())
}

fn merge_level(
    topo_map: &Mapping,
    user: &Document,
    user_map: Option<Mapping>,
    at_root: bool,
    level: &Value,
    path: &mut Vec<String>,
) -> Result<(), CliError> {
    for (key_node, value_node) in topo_map.iter() {
        let key = key_text(&key_node)?;
        let user_value: Option<&Value> = match level {
            Value::Map(map) => map.get(&key),
            _ => None,
        };
        let topo_child = value_node.as_mapping().filter(|child| !child.is_empty());
        if let (Some(topo_child), Some(Value::Map(_))) = (topo_child, user_value) {
            let child_cst = user_map
                .as_ref()
                .and_then(|mapping| mapping.get(&key))
                .and_then(|node| {
                    node.as_mapping()
                        .cloned()
                        .or_else(|| node.as_tagged()?.as_mapping())
                });
            path.push(key.clone());
            merge_level(
                topo_child,
                user,
                child_cst,
                false,
                user_value.expect("checked above"),
                path,
            )?;
            path.pop();
            continue;
        }
        let at_root = at_root && path.is_empty();
        if user_value.is_some() {
            if value_node
                .as_mapping()
                .is_some_and(|child| child.is_empty())
                && matches!(user_value, Some(Value::Map(_)))
            {
                // An empty topology map contributes nothing over an
                // existing map; replacing it wholesale would drop the
                // user's entries, so the live mapping survives as-is.
                continue;
            }
            if matches!(user_value, Some(Value::Map(_))) {
                // Existing (empty) map: replace wholesale, keeping the
                // topology node's comments, like the previous installer.
                set_value(user, user_map.as_ref(), at_root, &key, &value_node)?;
            } else {
                let plain = plain_value(&value_node).ok_or_else(|| {
                    CliError::Topology(format!(
                        "topology config value '{}' is not mergeable",
                        path.join(".")
                    ))
                })?;
                set_value(user, user_map.as_ref(), at_root, &key, &plain)?;
            }
        } else {
            // New key: graft the topology subtree with its comments intact.
            set_value(user, user_map.as_ref(), at_root, &key, &value_node)?;
        }
    }
    Ok(())
}

/// Walk `prefix` into the user document, returning the mapping there.
/// `None` when the prefix does not resolve to a mapping.
fn walk_mapping(doc: &Document, prefix: &[String]) -> Option<Mapping> {
    let mut current = doc.as_mapping();
    for part in prefix {
        current = current
            .and_then(|mapping| mapping.get(part))
            .and_then(|node| {
                node.as_mapping()
                    .cloned()
                    .or_else(|| node.as_tagged()?.as_mapping())
            });
    }
    current
}

fn remove_at(doc: &Document, parts: &[String]) -> Result<(), CliError> {
    let (last, prefix) = parts
        .split_last()
        .ok_or_else(|| CliError::Usage("internal error: empty config path".to_string()))?;
    match walk_mapping(doc, prefix) {
        Some(mapping) => {
            let _removed = mapping.remove(last);
            Ok(())
        }
        None => Err(CliError::Config(format!(
            "'{CONFIG_NAME}' key '{}' could not be matched exactly in the \
             live file; refusing to guess",
            parts.join(".")
        ))),
    }
}

/// Restore topology-owned config keys on the user document.
///
/// * `restored` carries the prior values to write back.
/// * `deleted` lists owned keys the user did not have before install, which
///   are removed outright.
/// * `emptied` lists ancestor mappings that became empty because of those
///   deletions (computed by the caller on the predicted result tree); they
///   are pruned last, deepest first.
///
/// Returns the re-serialized document; the caller skips the write when it
/// matches the original bytes.
pub fn restore_config(
    user: &Document,
    restored: &[(Vec<String>, Value)],
    deleted: &[Vec<String>],
    emptied: &[Vec<String>],
) -> Result<String, CliError> {
    for parts in deleted {
        remove_at(user, parts)?;
    }
    for (parts, prior) in restored {
        let (last, prefix) = parts
            .split_last()
            .ok_or_else(|| CliError::Usage("internal error: empty config path".to_string()))?;
        match walk_mapping(user, prefix) {
            Some(mapping) => {
                mapping.set(last.as_str(), value::to_yaml(prior));
            }
            None => {
                return Err(CliError::Config(format!(
                    "'{CONFIG_NAME}' key '{}' could not be matched exactly in \
                     the live file; refusing to guess",
                    parts.join(".")
                )));
            }
        }
    }
    for parts in emptied {
        remove_at(user, parts)?;
    }
    Ok(user.to_string())
}
