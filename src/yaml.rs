//! A dynamically-typed YAML value, deserializable via serde.

use std::convert::TryFrom;
use std::fmt;
use std::hash::{Hash, Hasher};

use linked_hash_map::LinkedHashMap;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// A YAML mapping, preserving key order.
pub type Mapping = LinkedHashMap<Value, Value>;

/// A YAML number.
#[derive(Clone, Copy, Debug)]
pub enum Number {
    /// A signed integer.
    Int(i64),
    /// An unsigned integer too large for `i64`.
    UInt(u64),
    /// A floating-point number.
    Float(f64),
}

impl Number {
    /// Returns the number as an `i64` if it is an integer that fits.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Number::Int(v) => Some(v),
            Number::UInt(v) => i64::try_from(v).ok(),
            Number::Float(_) => None,
        }
    }
    /// Returns the number as a `u64` if it is a non-negative integer.
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Number::Int(v) => u64::try_from(v).ok(),
            Number::UInt(v) => Some(v),
            Number::Float(_) => None,
        }
    }
}

impl PartialEq for Number {
    fn eq(&self, other: &Number) -> bool {
        match (*self, *other) {
            (Number::Int(a), Number::Int(b)) => a == b,
            (Number::UInt(a), Number::UInt(b)) => a == b,
            (Number::Float(a), Number::Float(b)) => a.to_bits() == b.to_bits(),
            _ => false,
        }
    }
}

impl Eq for Number {}

impl Hash for Number {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match *self {
            Number::Int(v) => (0u8, v).hash(state),
            Number::UInt(v) => (1u8, v).hash(state),
            Number::Float(v) => (2u8, v.to_bits()).hash(state),
        }
    }
}

/// A YAML value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Value {
    /// `null` or an empty value.
    Null,
    /// A boolean.
    Bool(bool),
    /// A number.
    Number(Number),
    /// A string.
    String(String),
    /// A sequence of values.
    Sequence(Vec<Value>),
    /// A mapping from values to values.
    Mapping(Mapping),
}

impl Value {
    /// Returns the mapping if this value is one.
    pub fn as_mapping(&self) -> Option<&Mapping> {
        match self {
            Value::Mapping(m) => Some(m),
            _ => None,
        }
    }
    /// Returns the mapping if this value is one.
    pub fn as_mapping_mut(&mut self) -> Option<&mut Mapping> {
        match self {
            Value::Mapping(m) => Some(m),
            _ => None,
        }
    }
    /// Looks up a string key if this value is a mapping.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_mapping()?.get(&Value::String(key.to_string()))
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
        struct ValueVisitor;

        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = Value;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any YAML value")
            }
            fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
                Ok(Value::Bool(v))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
                Ok(Value::Number(Number::Int(v)))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
                Ok(Value::Number(match i64::try_from(v) {
                    Ok(v) => Number::Int(v),
                    Err(_) => Number::UInt(v),
                }))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
                Ok(Value::Number(Number::Float(v)))
            }
            fn visit_str<E>(self, v: &str) -> Result<Value, E> {
                Ok(Value::String(v.to_string()))
            }
            fn visit_string<E>(self, v: String) -> Result<Value, E> {
                Ok(Value::String(v))
            }
            fn visit_unit<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_none<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
                Deserialize::deserialize(d)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
                let mut ret = Vec::new();
                while let Some(v) = seq.next_element()? {
                    ret.push(v);
                }
                Ok(Value::Sequence(ret))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
                let mut ret = Mapping::new();
                while let Some((k, v)) = map.next_entry()? {
                    if ret.insert(k, v).is_some() {
                        return Err(de::Error::custom("duplicate mapping key"));
                    }
                }
                Ok(Value::Mapping(ret))
            }
        }

        deserializer.deserialize_any(ValueVisitor)
    }
}
