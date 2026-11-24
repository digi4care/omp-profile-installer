//! Type-sensitive semantic model of YAML config values.
//!
//! The installer compares topology-owned values against the live config and
//! against values recorded in the ownership manifest. Manifest prior-value
//! restoration is type-strict: booleans never equal integers, integers
//! never equal floats, and strings never equal either, mirroring the
//! type-sensitive equality the previous yq-based installer applied to its
//! JSON decoding. The live-config conflict check (`type_conflicts`) is only
//! shape-strict: any scalar may replace any other scalar, and only
//! map/sequence shape mismatches are rejected.
//!
//! Complexity: `resolve_path` is O(depth of path); `type_conflicts` is
//! O(size of the topology config); `leaf_paths` is O(size of the topology
//! config). All are linear walks over already-parsed data with no repeated
//! scanning.

use std::collections::BTreeMap;

use yaml_edit::{ScalarType, YamlValue};

use crate::error::CliError;

/// A YAML value with a type distinction strict enough for ownership checks.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Seq(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl Value {
    /// `true` when this is an empty mapping.
    pub fn is_empty_map(&self) -> bool {
        matches!(self, Value::Map(map) if map.is_empty())
    }
}

/// Convert a (YAML 1.2-migrated) `YamlValue` into the strict model.
///
/// Timestamps, regexes, and unresolvable integers/floats degrade to strings;
// `!!set`/`!!omap`/`!!pairs` collections are rejected as unsupported.
pub fn from_yaml(value: &YamlValue) -> Result<Value, CliError> {
    match value {
        YamlValue::Scalar(scalar) => Ok(match scalar.scalar_type() {
            ScalarType::Null => Value::Null,
            ScalarType::Boolean => match scalar.to_bool() {
                Some(flag) => Value::Bool(flag),
                None => Value::Str(scalar.value().to_string()),
            },
            ScalarType::Integer => match scalar.to_i64() {
                Some(number) => Value::Int(number),
                None => Value::Str(scalar.value().to_string()),
            },
            ScalarType::Float => match scalar.to_f64() {
                Some(number) => Value::Float(number),
                None => Value::Str(scalar.value().to_string()),
            },
            _ => Value::Str(scalar.value().to_string()),
        }),
        YamlValue::Sequence(items) => {
            let mut converted = Vec::with_capacity(items.len());
            for item in items {
                converted.push(from_yaml(item)?);
            }
            Ok(Value::Seq(converted))
        }
        YamlValue::Mapping(map) => {
            let mut converted = BTreeMap::new();
            for (key, item) in map {
                converted.insert(key.clone(), from_yaml(item)?);
            }
            Ok(Value::Map(converted))
        }
        YamlValue::Tagged(_, inner) => from_yaml(inner),
        other => Err(CliError::Config(format!(
            "unsupported YAML collection in config: {other:?}"
        ))),
    }
}

/// Convert a manifest JSON value into the strict model.
///
/// JSON numbers that fit in `i64` become `Int`; every other number becomes
/// `Float`, keeping the int/float distinction the manifest records.
pub fn from_json(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(flag) => Value::Bool(*flag),
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Value::Int(integer)
            } else {
                Value::Float(number.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(text) => Value::Str(text.clone()),
        serde_json::Value::Array(items) => Value::Seq(items.iter().map(from_json).collect()),
        serde_json::Value::Object(map) => {
            let mut converted = BTreeMap::new();
            for (key, item) in map {
                converted.insert(key.clone(), from_json(item));
            }
            Value::Map(converted)
        }
    }
}

/// Convert a strict-model value back to a `YamlValue` for lossless editing.
///
/// Scalars are rebuilt with explicit types so a string "true" never becomes
/// a boolean and integers never render as floats.
pub fn to_yaml(value: &Value) -> YamlValue {
    match value {
        Value::Null => YamlValue::Scalar(yaml_edit::ScalarValue::null()),
        Value::Bool(flag) => YamlValue::Scalar(yaml_edit::ScalarValue::from(*flag)),
        Value::Int(number) => YamlValue::Scalar(yaml_edit::ScalarValue::from(*number)),
        Value::Float(number) => YamlValue::Scalar(yaml_edit::ScalarValue::from(*number)),
        Value::Str(text) => YamlValue::Scalar(yaml_edit::ScalarValue::string(text.clone())),
        Value::Seq(items) => YamlValue::Sequence(items.iter().map(to_yaml).collect()),
        Value::Map(map) => {
            let mut converted = BTreeMap::new();
            for (key, item) in map {
                converted.insert(key.clone(), to_yaml(item));
            }
            YamlValue::Mapping(converted)
        }
    }
}

/// Walk `parts` into `node`; `None` if any step is absent.
pub fn resolve_path<'a>(node: &'a Value, parts: &[String]) -> Option<&'a Value> {
    let mut current = node;
    for part in parts {
        match current {
            Value::Map(map) => current = map.get(part)?,
            _ => return None,
        }
    }
    Some(current)
}

/// Paths to scalar/sequence/empty-map leaves of a topology config mapping.
///
/// An empty map counts as a leaf so installing `foo: {}` records ownership
/// of the whole (absent or empty) key, exactly like the previous installer.
pub fn leaf_paths(node: &Value, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match node {
        Value::Map(map) if !map.is_empty() => {
            for (key, value) in map {
                prefix.push(key.clone());
                leaf_paths(value, prefix, out);
                prefix.pop();
            }
        }
        _ => out.push(prefix.clone()),
    }
}

/// Collect dot-paths where the value shape is incompatible between the
/// topology config and the existing user config, in sorted key order.
///
/// The comparison is structure-strict, not subtype-strict: nested maps are
/// compared recursively, sequences and scalars of any subtype
/// (boolean/string/number/null) are interchangeable, and only a shape
/// mismatch — e.g. a map where the user has a scalar — is reported. A
/// scalar topology value may replace a boolean, and vice versa, so string
/// spellings such as `always`/`never` coexist with YAML booleans.
pub fn type_conflicts(topo: &Value, user: &Value, prefix: &mut Vec<String>, out: &mut Vec<String>) {
    let Value::Map(topo_map) = topo else {
        return;
    };
    for (key, topo_value) in topo_map {
        prefix.push(key.clone());
        if let Some(user_value) = match user {
            Value::Map(user_map) => user_map.get(key),
            _ => None,
        } {
            match (topo_value, user_value) {
                (Value::Map(_), Value::Map(_)) => {
                    type_conflicts(topo_value, user_value, prefix, out);
                }
                (Value::Seq(_), Value::Seq(_)) => {}
                _ if is_scalar(topo_value) && is_scalar(user_value) => {}
                _ => out.push(format!(
                    "config key '{}' has incompatible type: topology expects \
                     {} but the existing value is a {}",
                    prefix.join("."),
                    type_name(topo_value),
                    type_name(user_value)
                )),
            }
        }
        prefix.pop();
    }
}

/// `true` for null/boolean/integer/float/string leaves (everything that is
/// not a sequence or mapping).
fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Seq(_) | Value::Map(_))
}

/// Fine-grained type name used in conflict messages.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Int(_) => "integer",
        Value::Float(_) => "float",
        Value::Str(_) => "string",
        Value::Seq(_) => "array",
        Value::Map(_) => "map",
    }
}

/// Render a value the way Python's `str()` rendered metadata fields, so
/// manifest `topology_version` strings stay stable across the port.
pub fn display_metadata(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::Bool(flag)) => {
            if *flag {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Some(Value::Int(number)) => number.to_string(),
        Some(Value::Float(number)) => {
            if number.fract() == 0.0 {
                format!("{number:.1}")
            } else {
                number.to_string()
            }
        }
        Some(Value::Str(text)) => text.clone(),
        Some(Value::Seq(items)) => items
            .iter()
            .map(|item| display_metadata(Some(item)))
            .collect::<Vec<_>>()
            .join(", "),
        Some(Value::Map(_)) => String::new(),
    }
}
