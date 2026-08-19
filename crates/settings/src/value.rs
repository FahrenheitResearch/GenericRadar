//! The value a setting holds, and its JSON form.
//!
//! Values rest on disk as plain JSON scalars - a bool, a number, a string -
//! rather than as a tagged enum, so a file is hand-readable and a future
//! build can add value shapes without breaking this one: anything this
//! build does not recognise is carried through the store untouched (the
//! store keeps raw JSON and only converts at the edges) and simply resolves
//! to the setting's default here.

use serde_json::Value as Json;

/// A typed setting value. Conversion to and from JSON is total in one
/// direction (every `SettingValue` has a JSON form) and partial in the other
/// (JSON arrays, objects and nulls have no `SettingValue`, and resolve to the
/// setting's default instead of failing the whole file).
#[derive(Clone, Debug, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl SettingValue {
    /// Read a value out of its JSON form. `None` for shapes no current
    /// setting kind uses; the caller falls back to the default.
    pub fn from_json(json: &Json) -> Option<Self> {
        match json {
            Json::Bool(value) => Some(Self::Bool(*value)),
            Json::Number(number) => {
                // Integers stay integers so an `Integer` setting round-trips
                // exactly; anything with a fraction is a float.
                if let Some(int) = number.as_i64() {
                    Some(Self::Int(int))
                } else {
                    number.as_f64().map(Self::Float)
                }
            }
            Json::String(text) => Some(Self::Text(text.clone())),
            Json::Null | Json::Array(_) | Json::Object(_) => None,
        }
    }

    pub fn to_json(&self) -> Json {
        match self {
            Self::Bool(value) => Json::Bool(*value),
            Self::Int(value) => Json::Number((*value).into()),
            Self::Float(value) => serde_json::Number::from_f64(*value)
                // A non-finite float has no JSON form. Storing null would
                // round-trip to "unset", which is the honest reading of a
                // value that cannot be written down.
                .map(Json::Number)
                .unwrap_or(Json::Null),
            Self::Text(value) => Json::String(value.clone()),
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// Integer reading. A float that carries a whole number is accepted, so a
    /// hand-edited `"max_frames": 30.0` does not silently reset to default.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            Self::Float(value) if value.fract() == 0.0 && value.is_finite() => {
                // f64 represents every integer up to 2^53 exactly; beyond that
                // the cast is still saturating and finite, which the range
                // clamp in the registry then bounds.
                Some(*value as i64)
            }
            _ => None,
        }
    }

    /// Float reading. Integers widen losslessly (for magnitudes any real
    /// setting uses), so `"dim": 1` reads as `1.0`.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(value) => Some(*value),
            Self::Int(value) => Some(*value as f64),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(value) => Some(value),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalars_round_trip_through_json() {
        for value in [
            SettingValue::Bool(true),
            SettingValue::Int(-3),
            SettingValue::Float(0.35),
            SettingValue::Text("slate".to_owned()),
        ] {
            assert_eq!(SettingValue::from_json(&value.to_json()), Some(value));
        }
    }

    #[test]
    fn unrecognised_json_shapes_read_as_unset_not_as_errors() {
        assert_eq!(SettingValue::from_json(&json!(null)), None);
        assert_eq!(SettingValue::from_json(&json!([1, 2])), None);
        assert_eq!(SettingValue::from_json(&json!({"nested": true})), None);
    }

    #[test]
    fn whole_floats_read_as_integers_and_integers_read_as_floats() {
        assert_eq!(SettingValue::Float(30.0).as_int(), Some(30));
        assert_eq!(SettingValue::Float(30.5).as_int(), None);
        assert_eq!(SettingValue::Int(1).as_float(), Some(1.0));
        assert_eq!(SettingValue::Bool(true).as_int(), None);
    }

    #[test]
    fn a_non_finite_float_serialises_to_null_which_reads_back_as_unset() {
        let json = SettingValue::Float(f64::NAN).to_json();
        assert_eq!(json, Json::Null);
        assert_eq!(SettingValue::from_json(&json), None);
    }
}
