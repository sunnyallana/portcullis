//! The value model that crosses the boundary between an agent, Portcullis and a
//! database.
//!
//! Values are deliberately narrow. Anything an action accepts or returns is one
//! of these eight shapes, which keeps type checking, masking and SQL binding
//! honest: there is no "whatever the driver gave us" escape hatch.

use std::fmt;
use std::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize, Serializer};

/// The declared type of a column or an action parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    /// True or false.
    Bool,
    /// 64-bit signed integer.
    Int,
    /// 64-bit float. Never use for money.
    Float,
    /// Exact decimal. Use for money.
    Decimal,
    /// UTF-8 text.
    Text,
    /// Instant in time, always stored and rendered as UTC.
    Timestamp,
    /// RFC 4122 UUID.
    Uuid,
    /// Arbitrary JSON document.
    Json,
}

impl DataType {
    /// Parse the spelling used in configuration files.
    ///
    /// Accepts friendly aliases so operators do not have to memorise the
    /// canonical name (`string` for `text`, `bigint` for `int`, and so on).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "bool" | "boolean" => Self::Bool,
            "int" | "integer" | "bigint" | "long" | "smallint" => Self::Int,
            "float" | "double" | "real" => Self::Float,
            "decimal" | "numeric" | "money" => Self::Decimal,
            "text" | "string" | "varchar" | "char" => Self::Text,
            "timestamp" | "datetime" | "timestamptz" | "date" => Self::Timestamp,
            "uuid" | "guid" => Self::Uuid,
            "json" | "jsonb" => Self::Json,
            _ => return None,
        })
    }

    /// The canonical name, as printed in diagnostics and tool schemas.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Float => "float",
            Self::Decimal => "decimal",
            Self::Text => "text",
            Self::Timestamp => "timestamp",
            Self::Uuid => "uuid",
            Self::Json => "json",
        }
    }

    /// The JSON Schema type an MCP client should send for this parameter.
    pub fn json_schema_type(self) -> &'static str {
        match self {
            Self::Bool => "boolean",
            Self::Int => "integer",
            Self::Float => "number",
            Self::Json => "object",
            Self::Decimal | Self::Text | Self::Timestamp | Self::Uuid => "string",
        }
    }

    /// Extra JSON Schema hints that help a model produce a valid argument.
    pub fn json_schema_format(self) -> Option<&'static str> {
        match self {
            Self::Timestamp => Some("date-time"),
            Self::Uuid => Some("uuid"),
            Self::Decimal => Some("decimal"),
            _ => None,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A single typed value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL NULL / JSON null.
    Null,
    /// Boolean.
    Bool(bool),
    /// Integer.
    Int(i64),
    /// Float.
    Float(f64),
    /// Exact decimal.
    Decimal(Decimal),
    /// Text.
    Text(String),
    /// UTC instant.
    Timestamp(jiff::Timestamp),
    /// UUID.
    Uuid(uuid::Uuid),
    /// JSON document.
    Json(serde_json::Value),
}

impl Value {
    /// The type of this value, or `None` for `Null`, which belongs to every type.
    pub fn type_of(&self) -> Option<DataType> {
        Some(match self {
            Self::Null => return None,
            Self::Bool(_) => DataType::Bool,
            Self::Int(_) => DataType::Int,
            Self::Float(_) => DataType::Float,
            Self::Decimal(_) => DataType::Decimal,
            Self::Text(_) => DataType::Text,
            Self::Timestamp(_) => DataType::Timestamp,
            Self::Uuid(_) => DataType::Uuid,
            Self::Json(_) => DataType::Json,
        })
    }

    /// Is this the null value?
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Convert an incoming JSON argument into a typed value.
    ///
    /// This is the only place untrusted input becomes a `Value`, so it is strict
    /// on purpose: a string is not silently coerced into an integer, and a
    /// decimal keeps every digit it arrived with.
    pub fn from_json(json: &serde_json::Value, ty: DataType) -> Result<Self, String> {
        use serde_json::Value as J;
        if json.is_null() {
            return Ok(Self::Null);
        }
        Ok(match (ty, json) {
            (DataType::Bool, J::Bool(b)) => Self::Bool(*b),
            (DataType::Int, J::Number(n)) => Self::Int(
                n.as_i64()
                    .ok_or_else(|| format!("{n} is not a 64-bit integer"))?,
            ),
            (DataType::Float, J::Number(n)) => {
                Self::Float(n.as_f64().ok_or_else(|| format!("{n} is not a float"))?)
            }
            (DataType::Decimal, J::String(s)) => Self::Decimal(
                Decimal::from_str_exact(s).map_err(|e| format!("`{s}` is not a decimal: {e}"))?,
            ),
            (DataType::Decimal, J::Number(n)) => Self::Decimal(
                Decimal::from_str_exact(&n.to_string())
                    .map_err(|e| format!("`{n}` is not an exact decimal: {e}"))?,
            ),
            (DataType::Text, J::String(s)) => Self::Text(s.clone()),
            (DataType::Timestamp, J::String(s)) => Self::Timestamp(
                jiff::Timestamp::from_str(s)
                    .map_err(|e| format!("`{s}` is not an RFC 3339 timestamp: {e}"))?,
            ),
            (DataType::Uuid, J::String(s)) => Self::Uuid(
                uuid::Uuid::parse_str(s).map_err(|e| format!("`{s}` is not a UUID: {e}"))?,
            ),
            (DataType::Json, other) => Self::Json(other.clone()),
            (want, got) => return Err(format!("expected {want}, received {}", json_kind(got))),
        })
    }

    /// Render as JSON for a tool result or an audit record.
    ///
    /// Decimals and timestamps become strings. JSON numbers cannot hold an
    /// arbitrary-precision decimal, and silently rounding someone's refund
    /// amount is not an acceptable failure mode.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Self::Null => J::Null,
            Self::Bool(b) => J::Bool(*b),
            Self::Int(i) => J::Number((*i).into()),
            Self::Float(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
            Self::Decimal(d) => J::String(d.to_string()),
            Self::Text(s) => J::String(s.clone()),
            Self::Timestamp(t) => J::String(t.to_string()),
            Self::Uuid(u) => J::String(u.to_string()),
            Self::Json(v) => v.clone(),
        }
    }

    /// Borrow as text, if this is text.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Interpret as an exact decimal, widening integers.
    ///
    /// Floats are refused: comparing an approval threshold against a float is a
    /// way to approve 500.0000000001 by accident.
    pub fn as_decimal(&self) -> Option<Decimal> {
        match self {
            Self::Decimal(d) => Some(*d),
            Self::Int(i) => Some(Decimal::from(*i)),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(i) => write!(f, "{i}"),
            Self::Float(x) => write!(f, "{x}"),
            Self::Decimal(d) => write!(f, "{d}"),
            Self::Text(s) => f.write_str(s),
            Self::Timestamp(t) => write!(f, "{t}"),
            Self::Uuid(u) => write!(f, "{u}"),
            Self::Json(v) => write!(f, "{v}"),
        }
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(s)
    }
}

/// Untyped deserialization, used for literals written in configuration files
/// (caller attributes, for example) where no column type is available yet.
///
/// Anything Portcullis cannot place natively becomes [`Value::Json`], which keeps
/// the value intact rather than guessing at a narrower type.
impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Value;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a scalar or JSON value")
            }

            fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
                Ok(Value::Bool(v))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
                Ok(Value::Int(v))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
                // Beyond i64 a float would round, so keep it exact as a decimal.
                Ok(i64::try_from(v).map_or_else(|_| Value::Decimal(v.into()), Value::Int))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
                Ok(Value::Float(v))
            }
            fn visit_str<E>(self, v: &str) -> Result<Value, E> {
                Ok(Value::Text(v.to_owned()))
            }
            fn visit_string<E>(self, v: String) -> Result<Value, E> {
                Ok(Value::Text(v))
            }
            fn visit_none<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_unit<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
                Value::deserialize(d)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<Value, A::Error> {
                let v = serde::de::value::SeqAccessDeserializer::new(seq);
                Ok(Value::Json(serde_json::Value::deserialize(v)?))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Value, A::Error> {
                let v = serde::de::value::MapAccessDeserializer::new(map);
                Ok(Value::Json(serde_json::Value::deserialize(v)?))
            }
        }

        d.deserialize_any(V)
    }
}

fn json_kind(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn money_never_goes_through_a_float() {
        let v = Value::from_json(&json!("1200.05"), DataType::Decimal).unwrap();
        assert_eq!(v.to_json(), json!("1200.05"));
        let v = Value::from_json(&json!(1200.05), DataType::Decimal).unwrap();
        assert_eq!(v.to_json(), json!("1200.05"));
    }

    #[test]
    fn wrong_json_kind_is_rejected_with_a_readable_message() {
        let err = Value::from_json(&json!("8812"), DataType::Int).unwrap_err();
        assert_eq!(err, "expected int, received a string");
    }

    #[test]
    fn null_is_accepted_for_every_type() {
        for ty in [DataType::Int, DataType::Text, DataType::Json] {
            assert!(Value::from_json(&json!(null), ty).unwrap().is_null());
        }
    }

    #[test]
    fn float_is_not_a_decimal_for_threshold_purposes() {
        assert!(Value::Float(500.0).as_decimal().is_none());
        assert_eq!(
            Value::Int(500).as_decimal(),
            Decimal::from_str_exact("500").ok()
        );
    }
}
