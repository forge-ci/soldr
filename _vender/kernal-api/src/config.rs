//! Configuration mechanics only; applications own field validation and defaults.

use std::collections::BTreeMap;

/// Maximum UTF-8 source bytes, checked before invoking the parser.
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;
/// Maximum decoded values, counting the root table and every container.
pub const MAX_NODES: usize = 16_384;
/// Maximum value depth, with the root table at depth zero.
pub const MAX_DEPTH: usize = 32;

/// Semantic configuration values independent of the private TOML parser.
/// Table keys are ordered; source formatting and comments are not retained.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    /// Canonical TOML date/time spelling; no timezone conversion is performed.
    DateTime(String),
    Array(Vec<Value>),
    Table(BTreeMap<String, Value>),
}

/// A parsed configuration. Construction enforces bounds before it is returned.
#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    root: Value,
}

/// Bounded diagnostics that never echo configuration contents or secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ParseError {
    #[error("configuration exceeds 1048576 UTF-8 source bytes")]
    InputTooLarge,
    #[error("invalid TOML document")]
    InvalidSyntax,
    #[error("configuration exceeds 16384 decoded values")]
    TooManyNodes,
    #[error("configuration exceeds value depth 32")]
    TooDeep,
}

impl Document {
    /// Decode a TOML document without filesystem access or interpolation.
    ///
    /// The source byte bound applies before parsing. Node/depth bounds apply
    /// after the private parser constructs its tree, during conversion to owned
    /// semantic values. They are not independent parser CPU or allocation quotas;
    /// source size and the parser's own recursion limit bound that earlier stage.
    /// No partial document is returned on error. Unknown fields remain values
    /// for application policy; empty input is a valid empty table.
    pub fn parse_toml(source: &str) -> Result<Self, ParseError> {
        if source.len() > MAX_INPUT_BYTES {
            return Err(ParseError::InputTooLarge);
        }
        let table = toml::from_str::<toml::Table>(source).map_err(|_| ParseError::InvalidSyntax)?;
        let mut remaining = MAX_NODES;
        Ok(Self {
            root: convert(toml::Value::Table(table), 0, &mut remaining)?,
        })
    }

    /// The root of a TOML document is always a table.
    pub fn root(&self) -> &Value {
        &self.root
    }
}

fn convert(value: toml::Value, depth: usize, remaining: &mut usize) -> Result<Value, ParseError> {
    if depth > MAX_DEPTH {
        return Err(ParseError::TooDeep);
    }
    *remaining = remaining.checked_sub(1).ok_or(ParseError::TooManyNodes)?;
    Ok(match value {
        toml::Value::String(value) => Value::String(value),
        toml::Value::Integer(value) => Value::Integer(value),
        toml::Value::Float(value) => Value::Float(value),
        toml::Value::Boolean(value) => Value::Boolean(value),
        toml::Value::Datetime(value) => Value::DateTime(value.to_string()),
        toml::Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| convert(value, depth + 1, remaining))
                .collect::<Result<_, _>>()?,
        ),
        toml::Value::Table(values) => Value::Table(
            values
                .into_iter()
                .map(|(key, value)| Ok((key, convert(value, depth + 1, remaining)?)))
                .collect::<Result<_, ParseError>>()?,
        ),
    })
}
