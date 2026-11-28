//! Bounded JSON mechanics. Applications own schemas, defaults and field policy.

use std::collections::BTreeMap;
use std::io::Write;

/// Source byte limit, enforced before parsing.
pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
/// Encoded byte limit, including escaping and whitespace.
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum decoded values, counting containers and the root (not object keys).
pub const MAX_NODES: usize = 262_144;
/// Maximum value depth; the root has depth zero.
pub const MAX_DEPTH: usize = 64;

/// Owned JSON values, independent of the private serialization backend.
/// Map objects encode in key order; member objects preserve order and duplicates.
/// Positive parsed integers use `Signed` when
/// representable, otherwise `Unsigned`; numeric variant identity is not a
/// wire-format guarantee. Floating-point values must be finite when encoded.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
    /// Unmerged members from [`parse_members`], in source order. Applications
    /// decide which duplicate fields their schemas accept. Encoding preserves
    /// all entries; it does not merge them into a map.
    ObjectMembers(Vec<(String, Value)>),
}

/// Output style; neither style appends a trailing newline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Layout {
    Compact,
    /// Two-space indentation.
    Pretty,
}

/// Bounded diagnostics that do not echo document contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("JSON source exceeds byte limit")]
    InputTooLarge,
    #[error("invalid JSON document")]
    InvalidSyntax,
    #[error("JSON exceeds value count limit")]
    TooManyNodes,
    #[error("JSON exceeds depth limit")]
    TooDeep,
    #[error("JSON numbers must be finite")]
    NonFiniteNumber,
    #[error("JSON output exceeds byte limit")]
    OutputTooLarge,
    #[error("JSON encoding failed")]
    EncodingFailed,
}

/// Parse one UTF-8 JSON value; trailing whitespace is allowed, trailing values
/// are not. Duplicate object keys retain their last value.
///
/// The input byte limit is checked before parsing. Value count and depth limits
/// apply to the decoded tree after private parsing, not as independent parser
/// allocation or CPU quotas. The parser also retains its own recursion limit.
/// No partial result is returned. Integers outside i64/u64 may decode as finite
/// f64 values with rounding; this is not an arbitrary-precision number API.
pub fn parse(source: &[u8]) -> Result<Value, Error> {
    if source.len() > MAX_INPUT_BYTES {
        return Err(Error::InputTooLarge);
    }
    let value = serde_json::from_slice(source).map_err(|_| Error::InvalidSyntax)?;
    let mut remaining = MAX_NODES;
    convert(value, 0, &mut remaining)
}

/// Parse without merging object members, including objects nested in arrays or
/// other objects. Every object becomes [`Value::ObjectMembers`]. Scalars and
/// arrays have the same representation as [`parse`].
///
/// The input byte limit applies before parsing. Depth and node limits apply
/// during decoding, counting every member value even when keys repeat. Keys
/// are not nodes. There is no second tree or duplicate-key index. These bounds
/// are not independent CPU or allocator quotas; individual strings and keys
/// are also bounded by the source byte limit. No partial result is returned.
/// Private borrowed raw-value syntax validation precedes member decoding and
/// can revisit nested source slices, bounded by input size and accepted depth.
pub fn parse_members(source: &[u8]) -> Result<Value, Error> {
    use serde::de::DeserializeSeed;
    if source.len() > MAX_INPUT_BYTES {
        return Err(Error::InputTooLarge);
    }
    let mut state = MemberState {
        remaining: MAX_NODES,
        failure: None,
    };
    let mut parser = serde_json::Deserializer::from_slice(source);
    let result = MemberSeed {
        state: &mut state,
        depth: 0,
    }
    .deserialize(&mut parser);
    let value = result.map_err(|_| state.failure.unwrap_or(Error::InvalidSyntax))?;
    parser.end().map_err(|_| Error::InvalidSyntax)?;
    Ok(value)
}

struct MemberState {
    remaining: usize,
    failure: Option<Error>,
}

struct MemberSeed<'a> {
    state: &'a mut MemberState,
    depth: usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for MemberSeed<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        if let Err(error) = visit(self.depth, &mut self.state.remaining) {
            self.state.failure = Some(error);
            return Err(serde::de::Error::custom("JSON resource limit"));
        }
        // A private raw slice distinguishes actual objects from the synthetic
        // map used by serde_json when arbitrary_precision is feature-unified.
        // Never interpret a user-controlled object key as a backend marker.
        let raw: &'de serde_json::value::RawValue = serde::Deserialize::deserialize(deserializer)?;
        let mut parser = serde_json::Deserializer::from_str(raw.get());
        use serde::Deserializer;
        match raw.get().as_bytes().first() {
            Some(b'{') => parser
                .deserialize_map(self)
                .map_err(serde::de::Error::custom),
            Some(b'[') => parser
                .deserialize_seq(self)
                .map_err(serde::de::Error::custom),
            _ => {
                let value = serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
                let mut scalar_budget = 1;
                convert(value, 0, &mut scalar_budget).map_err(serde::de::Error::custom)
            }
        }
    }
}

impl<'de> serde::de::Visitor<'de> for MemberSeed<'_> {
    type Value = Value;
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(MemberSeed {
            state: &mut *self.state,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut object: A) -> Result<Value, A::Error> {
        let mut members = Vec::new();
        while let Some(key) = object.next_key::<String>()? {
            let value = object.next_value_seed(MemberSeed {
                state: &mut *self.state,
                depth: self.depth + 1,
            })?;
            members.push((key, value));
        }
        Ok(Value::ObjectMembers(members))
    }
}

fn visit(depth: usize, remaining: &mut usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::TooDeep);
    }
    *remaining = remaining.checked_sub(1).ok_or(Error::TooManyNodes)?;
    Ok(())
}

fn convert(value: serde_json::Value, depth: usize, remaining: &mut usize) -> Result<Value, Error> {
    visit(depth, remaining)?;
    Ok(match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(value),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Value::Signed(value)
            } else if let Some(value) = value.as_u64() {
                Value::Unsigned(value)
            } else {
                Value::Float(value.as_f64().ok_or(Error::InvalidSyntax)?)
            }
        }
        serde_json::Value::String(value) => Value::String(value),
        serde_json::Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| convert(value, depth + 1, remaining))
                .collect::<Result<_, _>>()?,
        ),
        serde_json::Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| Ok((key, convert(value, depth + 1, remaining)?)))
                .collect::<Result<_, Error>>()?,
        ),
    })
}

fn validate(value: &Value, depth: usize, remaining: &mut usize) -> Result<(), Error> {
    visit(depth, remaining)?;
    match value {
        Value::Float(value) if !value.is_finite() => return Err(Error::NonFiniteNumber),
        Value::Array(values) => {
            for value in values {
                validate(value, depth + 1, remaining)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate(value, depth + 1, remaining)?;
            }
        }
        Value::ObjectMembers(values) => {
            for (_, value) in values {
                validate(value, depth + 1, remaining)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// Only this private borrowed adapter implements the backend trait. Encoding
// does not clone caller strings or construct a second tree.
struct Borrowed<'a>(&'a Value);

impl serde::Serialize for Borrowed<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self.0 {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Signed(value) => serializer.serialize_i64(*value),
            Value::Unsigned(value) => serializer.serialize_u64(*value),
            Value::Float(value) => serializer.serialize_f64(*value),
            Value::String(value) => serializer.serialize_str(value),
            Value::Array(values) => {
                let mut output = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    output.serialize_element(&Borrowed(value))?;
                }
                output.end()
            }
            Value::Object(values) => {
                let mut output = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    output.serialize_entry(key, &Borrowed(value))?;
                }
                output.end()
            }
            Value::ObjectMembers(values) => {
                let mut output = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    output.serialize_entry(key, &Borrowed(value))?;
                }
                output.end()
            }
        }
    }
}

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_OUTPUT_BYTES - self.bytes.len() {
            self.exceeded = true;
            return Err(std::io::Error::other("JSON output limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Encode a caller-owned value, checking depth/count/finite numbers first and
/// enforcing the output byte limit during serialization. No tree/string clone
/// or partial output is returned. These bounds do not limit caller allocations
/// made while constructing a value before this call.
pub fn encode(value: &Value, layout: Layout) -> Result<Vec<u8>, Error> {
    let mut remaining = MAX_NODES;
    validate(value, 0, &mut remaining)?;
    let mut output = Output::default();
    let result = match layout {
        Layout::Compact => serde_json::to_writer(&mut output, &Borrowed(value)),
        Layout::Pretty => serde_json::to_writer_pretty(&mut output, &Borrowed(value)),
    };
    if output.exceeded {
        return Err(Error::OutputTooLarge);
    }
    result.map_err(|_| Error::EncodingFailed)?;
    Ok(output.bytes)
}
