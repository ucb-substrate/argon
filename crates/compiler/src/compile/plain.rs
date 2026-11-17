//! Plain data: integers, booleans, strings, constant floats, and sequences,
//! tuples, structs, and enums of them. A plain value has no geometry, signals,
//! or solver variables, so it reads the same in every cell, and it crosses
//! cell boundaries as instance fields and cell arguments shared rather than
//! copied.

use std::hash::Hasher;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::*;

/// The constant of `expr` if it has no variables and solving it gives the
/// constant back unchanged. Solving adds the constant to `0.`, which turns
/// `-0.` into `0.`, so `-0.` is not plain.
fn plain_float(expr: &LinearExpr) -> Option<f64> {
    (expr.coeffs.is_empty() && (0. + expr.constant).to_bits() == expr.constant.to_bits())
        .then_some(expr.constant)
}

/// Whether `value` is plain data.
fn is_plain(value: &Value) -> bool {
    match value {
        Value::Int(_) | Value::Bool(_) | Value::String(_) => true,
        Value::Linear(expr) => plain_float(expr).is_some(),
        Value::Seq(items) => items.iter().all(is_plain),
        Value::Tuple(items) => items.iter().all(is_plain),
        Value::Struct(value) => value.values().iter().all(is_plain),
        Value::Enum(value) => match &value.payload {
            VariantValues::Tuple(items) => items.iter().all(is_plain),
            VariantValues::Struct(fields) => fields.values().all(is_plain),
        },
        _ => false,
    }
}

/// Whether `value` is plain data worth sharing: a sequence, tuple, struct, or
/// enum, rather than a scalar that is as cheap to copy.
pub(super) fn is_shared(value: &Value) -> bool {
    matches!(
        value,
        Value::Seq(_) | Value::Tuple(_) | Value::Struct(_) | Value::Enum(_)
    ) && is_plain(value)
}

/// A plain field value, shared with the value the cell bound. It serializes
/// as the [`FieldValue`] tree it stands for.
#[derive(Debug, Clone)]
pub struct PlainField(pub(super) Value);

impl PlainField {
    /// The field this value stands for.
    pub fn to_field(&self) -> FieldValue {
        field_of(&self.0)
    }
}

fn field_of(value: &Value) -> FieldValue {
    let fields = |fields: &mut dyn Iterator<Item = (&String, &Value)>| {
        fields
            .map(|(name, value)| (name.clone(), field_of(value)))
            .collect()
    };
    match value {
        Value::Int(i) => FieldValue::Int(*i),
        Value::Bool(b) => FieldValue::Bool(*b),
        Value::String(s) => FieldValue::String(s.clone()),
        Value::Linear(expr) => FieldValue::Float(expr.constant),
        Value::Seq(items) => FieldValue::Seq(items.iter().map(field_of).collect()),
        Value::Tuple(items) => FieldValue::Tuple(items.iter().map(field_of).collect()),
        Value::Struct(value) => FieldValue::Struct {
            name: value.name().to_owned(),
            fields: fields(&mut value.fields()),
        },
        Value::Enum(value) => match &value.payload {
            VariantValues::Tuple(items) => FieldValue::Enum {
                variant: value.variant.clone(),
                payload: items.iter().map(field_of).collect(),
            },
            VariantValues::Struct(named) => FieldValue::StructVariant {
                variant: value.variant.clone(),
                fields: fields(&mut named.iter()),
            },
        },
        _ => unreachable!("not plain data"),
    }
}

/// The plain value `field` stands for, or `None` if it holds anything else.
fn value_of(field: &FieldValue) -> Option<Value> {
    let all = |items: &[FieldValue]| items.iter().map(value_of).collect::<Option<Vec<_>>>();
    let named = |fields: &[(String, FieldValue)]| {
        fields
            .iter()
            .map(|(name, field)| Some((name.clone(), value_of(field)?)))
            .collect::<Option<Vec<_>>>()
    };
    Some(match field {
        FieldValue::Int(i) => Value::Int(*i),
        FieldValue::Bool(b) => Value::Bool(*b),
        FieldValue::String(s) => Value::String(s.clone()),
        FieldValue::Float(f) => Value::Linear(LinearExpr::from(*f)),
        FieldValue::Seq(items) => seq_value(all(items)?.into_iter().collect()),
        FieldValue::Tuple(items) => Value::Tuple(all(items)?.into()),
        FieldValue::Struct { name, fields } => Value::Struct(Arc::new(StructValue::from_fields(
            name.clone(),
            named(fields)?,
        ))),
        FieldValue::Enum { variant, payload } => Value::Enum(Arc::new(EnumValue {
            variant: variant.clone(),
            payload: VariantValues::Tuple(all(payload)?),
        })),
        FieldValue::StructVariant { variant, fields } => Value::Enum(Arc::new(EnumValue {
            variant: variant.clone(),
            payload: VariantValues::Struct(named(fields)?.into_iter().collect()),
        })),
        FieldValue::Plain(plain) => plain.0.clone(),
        _ => return None,
    })
}

impl Serialize for PlainField {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_field().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PlainField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let field = FieldValue::deserialize(deserializer)?;
        value_of(&field)
            .map(Self)
            .ok_or_else(|| D::Error::custom("expected plain data"))
    }
}

/// The argument `value` stands for, as a cell given it would see it unshared.
pub(super) fn cell_arg(value: &Value) -> CellArg {
    let fields = |fields: &mut dyn Iterator<Item = (&String, &Value)>| {
        fields
            .map(|(name, value)| (name.clone(), cell_arg(value)))
            .collect()
    };
    match value {
        Value::Int(i) => CellArg::Int(*i),
        Value::Bool(b) => CellArg::Bool(*b),
        Value::String(s) => CellArg::String(s.clone()),
        Value::Linear(expr) => CellArg::Float(expr.constant),
        Value::Seq(items) => CellArg::Seq(items.iter().map(cell_arg).collect()),
        Value::Tuple(items) => CellArg::Tuple(items.iter().map(cell_arg).collect()),
        Value::Struct(value) => CellArg::Struct {
            name: value.name().to_owned(),
            fields: fields(&mut value.fields()),
        },
        Value::Enum(value) => match &value.payload {
            VariantValues::Tuple(items) => CellArg::Enum {
                variant: value.variant.clone(),
                payload: items.iter().map(cell_arg).collect(),
            },
            VariantValues::Struct(named) => CellArg::StructVariant {
                variant: value.variant.clone(),
                fields: fields(&mut named.iter()),
            },
        },
        _ => unreachable!("not plain data"),
    }
}

/// A plain argument as part of a cell's cache key. It hashes and compares as
/// the [`CellArgKey`] tree it stands for would.
#[derive(Debug, Clone)]
pub(crate) struct PlainKey(pub(super) Value);

impl PlainKey {
    /// Feeds `hasher` what `gdscache` feeds it for the [`CellArgKey`] tree
    /// this value stands for.
    pub(crate) fn hash_expanded(&self, hasher: &mut impl Hasher) {
        hash_expanded(hasher, &self.0);
    }
}

fn hash_str(hasher: &mut impl Hasher, s: &str) {
    hasher.write_usize(s.len());
    hasher.write(s.as_bytes());
}

fn hash_expanded(hasher: &mut impl Hasher, value: &Value) {
    match value {
        Value::Linear(expr) => {
            hasher.write_u8(0);
            hasher.write_u64(expr.constant.to_bits());
        }
        Value::Int(i) => {
            hasher.write_u8(1);
            hasher.write_i64(*i);
        }
        Value::Bool(b) => {
            hasher.write_u8(2);
            hasher.write_u8(u8::from(*b));
        }
        Value::String(s) => {
            hasher.write_u8(3);
            hash_str(hasher, s);
        }
        Value::Seq(items) => {
            hasher.write_u8(5);
            hasher.write_usize(items.len());
            for item in items.iter() {
                hash_expanded(hasher, item);
            }
        }
        Value::Struct(value) => {
            hasher.write_u8(6);
            hash_str(hasher, value.name());
            hasher.write_usize(value.values().len());
            for (name, value) in value.fields() {
                hash_str(hasher, name);
                hash_expanded(hasher, value);
            }
        }
        Value::Tuple(items) => {
            hasher.write_u8(11);
            hasher.write_usize(items.len());
            for item in items.iter() {
                hash_expanded(hasher, item);
            }
        }
        Value::Enum(value) => match &value.payload {
            VariantValues::Tuple(items) => {
                hasher.write_u8(4);
                hash_str(hasher, &value.variant);
                hasher.write_usize(items.len());
                for item in items {
                    hash_expanded(hasher, item);
                }
            }
            VariantValues::Struct(fields) => {
                hasher.write_u8(12);
                hash_str(hasher, &value.variant);
                hasher.write_usize(fields.len());
                for (name, value) in fields {
                    hash_str(hasher, name);
                    hash_expanded(hasher, value);
                }
            }
        },
        _ => unreachable!("not plain data"),
    }
}

impl std::hash::Hash for PlainKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_expanded(state, &self.0);
    }
}

impl PartialEq for PlainKey {
    fn eq(&self, other: &Self) -> bool {
        plain_eq(&self.0, &other.0)
    }
}

impl Eq for PlainKey {}

/// Whether two plain values are the same, comparing floats by their bits as
/// [`CellArgKey`] does.
fn plain_eq(a: &Value, b: &Value) -> bool {
    let all = |a: &mut dyn ExactSizeIterator<Item = &Value>,
               b: &mut dyn ExactSizeIterator<Item = &Value>| {
        a.len() == b.len() && a.zip(b).all(|(a, b)| plain_eq(a, b))
    };
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Linear(a), Value::Linear(b)) => a.constant.to_bits() == b.constant.to_bits(),
        (Value::Seq(a), Value::Seq(b)) => Arc::ptr_eq(a, b) || all(&mut a.iter(), &mut b.iter()),
        (Value::Tuple(a), Value::Tuple(b)) => {
            Arc::ptr_eq(a, b) || all(&mut a.iter(), &mut b.iter())
        }
        (Value::Struct(a), Value::Struct(b)) => {
            Arc::ptr_eq(a, b)
                || (a.name() == b.name()
                    && a.values().len() == b.values().len()
                    && a.fields()
                        .zip(b.fields())
                        .all(|((na, va), (nb, vb))| na == nb && plain_eq(va, vb)))
        }
        (Value::Enum(a), Value::Enum(b)) => {
            Arc::ptr_eq(a, b)
                || (a.variant == b.variant
                    && match (&a.payload, &b.payload) {
                        (VariantValues::Tuple(a), VariantValues::Tuple(b)) => {
                            all(&mut a.iter(), &mut b.iter())
                        }
                        (VariantValues::Struct(a), VariantValues::Struct(b)) => {
                            a.len() == b.len()
                                && a.iter()
                                    .zip(b)
                                    .all(|((na, va), (nb, vb))| na == nb && plain_eq(va, vb))
                        }
                        _ => false,
                    })
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        let item = |k: i64| {
            Value::Struct(Arc::new(StructValue::from_fields(
                "m::Item".to_owned(),
                [
                    ("k".to_owned(), Value::Int(k)),
                    (
                        "x".to_owned(),
                        Value::Linear(LinearExpr::from(k as f64 * 0.5)),
                    ),
                    ("on".to_owned(), Value::Bool(k % 2 == 0)),
                    (
                        "tag".to_owned(),
                        Value::Enum(Arc::new(EnumValue {
                            variant: "m::Tag::A".to_owned(),
                            payload: VariantValues::Tuple(vec![Value::String("s".to_owned())]),
                        })),
                    ),
                    (
                        "pair".to_owned(),
                        Value::Tuple(vec![Value::Int(k), Value::Int(-k)].into()),
                    ),
                ],
            )))
        };
        seq_value((0..4).map(item).collect())
    }

    #[test]
    fn plain_keys_hash_like_the_trees_they_stand_for() {
        let value = sample();
        assert!(is_shared(&value));
        let mut shared = fnv::FnvHasher::default();
        PlainKey(value.clone()).hash_expanded(&mut shared);
        let expanded = crate::gdscache::cell_arg_key_hash(&CellArgKey::from(&cell_arg(&value)));
        assert_eq!(shared.finish(), expanded);
    }

    #[test]
    fn plain_fields_round_trip_through_serialization() {
        let field = FieldValue::Plain(PlainField(sample()));
        let bytes = postcard::to_stdvec(&field).unwrap();
        let back: FieldValue = postcard::from_bytes(&bytes).unwrap();
        let FieldValue::Plain(back) = back else {
            panic!("expected a plain field");
        };
        assert!(plain_eq(&back.0, &sample()));
    }

    #[test]
    fn negative_zero_is_not_plain() {
        assert!(!is_plain(&Value::Linear(LinearExpr::from(-0.))));
        assert!(is_plain(&Value::Linear(LinearExpr::from(0.))));
    }
}
