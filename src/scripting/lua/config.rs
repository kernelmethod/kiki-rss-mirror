//! Converts a script's config from JSON into the Lua table its top-level chunk receives.
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

fn to_lua_value(lua: &Lua, value: &Value) -> LuaResult<LuaValue> {
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
