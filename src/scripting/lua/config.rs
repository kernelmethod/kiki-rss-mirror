//! Converts a script's config from JSON into the Lua table its top-level chunk receives, and
//! Lua values into JSON for a plugin's store.
//!
//! JSON objects become tables keyed by string, and arrays become sequences indexed from 1.
//! Integers that fit in an `i64` become Lua integers and other numbers become floats.
//! `null` becomes `nil`, so an object key whose value is `null` is simply absent from its
//! table, and a `null` in an array leaves a hole in the sequence.

use mlua::prelude::*;
use serde_json::{Map, Value};

/// Converts a script's config into a Lua table.
///
/// The table is built inside the VM, so it counts against the VM's memory cap. Its depth is
/// bounded by `serde_json`'s recursion limit, which the config had to pass to be parsed.
///
/// # Errors
///
/// Returns an error if the VM cannot allocate the table, e.g. because the config would take
/// it over its memory cap.
pub(super) fn to_lua_table(lua: &Lua, config: &Map<String, Value>) -> LuaResult<LuaTable> {
    let table = lua.create_table_with_capacity(0, config.len())?;
    for (key, value) in config {
        table.raw_set(key.as_str(), to_lua_value(lua, value)?)?;
    }
    Ok(table)
}

pub(super) fn to_lua_value(lua: &Lua, value: &Value) -> LuaResult<LuaValue> {
    Ok(match value {
        Value::Null => LuaValue::Nil,
        Value::Bool(b) => LuaValue::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => LuaValue::Integer(i),
            None => LuaValue::Number(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => LuaValue::String(lua.create_string(s)?),
        Value::Array(items) => {
            let table = lua.create_table_with_capacity(items.len(), 0)?;
            for (i, item) in items.iter().enumerate() {
                table.raw_set(i + 1, to_lua_value(lua, item)?)?;
            }
            LuaValue::Table(table)
        }
        Value::Object(map) => LuaValue::Table(to_lua_table(lua, map)?),
    })
}

/// How deeply nested a Lua table may be to be converted to JSON.
const MAX_DEPTH: usize = 32;

/// Converts a Lua value into JSON, for keeping in a plugin's store.
///
/// `nil` becomes `null`, booleans, numbers and strings map across, and tables become
/// arrays if their keys are exactly `1..n`, and objects otherwise. An empty table becomes
/// an empty array.
///
/// # Errors
///
/// Returns an error for values with no JSON equivalent: functions and other userdata,
/// strings that are not UTF-8, `inf` and `nan`, tables with keys that are neither strings
/// nor a sequence, and tables nested more than 32 deep (which includes cyclic ones).
pub(super) fn from_lua_value(value: &LuaValue) -> LuaResult<Value> {
    from_lua_value_at(value, 0)
}

fn from_lua_value_at(value: &LuaValue, depth: usize) -> LuaResult<Value> {
    let err = |message: &str| LuaError::RuntimeError(message.to_string());
    Ok(match value {
        LuaValue::Nil => Value::Null,
        LuaValue::Boolean(b) => Value::Bool(*b),
        LuaValue::Integer(i) => Value::from(*i),
        LuaValue::Number(n) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .ok_or_else(|| err("cannot store a number that is not finite"))?,
        LuaValue::String(s) => Value::String(
            s.to_str()
                .map_err(|_| err("cannot store a string that is not UTF-8"))?
                .to_string(),
        ),
        LuaValue::Table(t) => {
            if depth >= MAX_DEPTH {
                return Err(err("cannot store tables nested more than 32 deep"));
            }
            let len = t.raw_len();
            let count = t.clone().pairs::<LuaValue, LuaValue>().count();
            if count == len {
                let mut items = Vec::with_capacity(len);
                for i in 1..=len {
                    items.push(from_lua_value_at(&t.raw_get::<LuaValue>(i)?, depth + 1)?);
                }
                Value::Array(items)
            } else {
                let mut map = Map::new();
                for pair in t.clone().pairs::<LuaValue, LuaValue>() {
                    let (k, v) = pair?;
                    let LuaValue::String(k) = k else {
                        return Err(err(
                            "cannot store a table whose keys are neither strings nor 1..n",
                        ));
                    };
                    let k = k
                        .to_str()
                        .map_err(|_| err("cannot store a key that is not UTF-8"))?
                        .to_string();
                    map.insert(k, from_lua_value_at(&v, depth + 1)?);
                }
                Value::Object(map)
            }
        }
        other => {
            return Err(LuaError::RuntimeError(format!(
                "cannot store a value of type {}",
                other.type_name()
            )))
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lua_values_round_trip_through_json() {
        let lua = Lua::new();
        for value in [
            json!(null),
            json!(true),
            json!(3),
            json!(0.5),
            json!("s"),
            json!([1, "a", [true]]),
            json!({"a": {"b": [1, 2]}}),
        ] {
            let back = from_lua_value(&to_lua_value(&lua, &value).unwrap()).unwrap();
            assert_eq!(back, value);
        }
    }

    #[test]
    fn values_without_json_equivalents_are_rejected() {
        let lua = Lua::new();
        for code in [
            "return function() end",
            "return 1/0",
            "return {[true] = 1}",
            "local t = {}; t.t = t; return t",
        ] {
            let value: LuaValue = lua.load(code).eval().unwrap();
            assert!(from_lua_value(&value).is_err(), "{code}");
        }
    }
}
