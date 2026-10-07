//! The `kiki.regex` API: regular expressions for scripts, backed by the `regex` crate.
//!
//! Lua's own patterns have no alternation and no case-insensitive matching, which rules
//! out most of what a filter wants to say. `kiki.regex(pattern [, flags])` compiles a
//! pattern with the `regex` crate and returns an object whose methods match it against
//! strings. The crate matches in time linear in the input, so a script matching untrusted
//! feed content cannot be made to backtrack for ever.
//!
//! # Resource limits
//!
//! Compiled regexes live outside the Lua allocator, so the VM's memory cap does not see
//! them. They are bounded instead as [`crate::scripting::regex`] describes, which compiles
//! them as it does for WebAssembly plugins: at most [`MAX_LIVE_REGEXES`] distinct regexes
//! may be alive in one VM at a time. Compiling
//! the same pattern and flags again returns the regex already compiled, so a handler that
//! builds its regexes on every call costs no more than one that builds them once.

use crate::scripting::regex::MAX_LIVE_REGEXES;
use mlua::prelude::*;
use regex::bytes::Regex;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

/// The regexes compiled in one VM, keyed by pattern and flags.
///
/// Entries are weak, so a regex is freed once Lua collects every object that refers to it;
/// the entry itself is removed when the regex is dropped.
type Registry = Arc<Mutex<HashMap<(String, String), Weak<Compiled>>>>;

/// A compiled regex, shared by every Lua object made from the same pattern and flags.
struct Compiled {
    regex: Regex,
    pattern: String,
    flags: String,
    registry: Registry,
}

impl Drop for Compiled {
    fn drop(&mut self) {
        let mut map = self
            .registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (
            std::mem::take(&mut self.pattern),
            std::mem::take(&mut self.flags),
        );
        // Only remove the entry if it is still ours: another regex for the same key may
        // already have replaced it.
        if map.get(&key).is_some_and(|w| w.strong_count() == 0) {
            map.remove(&key);
        }
    }
}

/// The Lua object returned by `kiki.regex`.
struct LuaRegex(Arc<Compiled>);

/// Install `kiki.regex` into the `kiki` table.
///
/// `kiki.regex` is a table that can be called like a function: `kiki.regex(pattern, flags)`
/// is the same as `kiki.regex.new(pattern, flags)`. It also carries `kiki.regex.escape`.
pub(super) fn install(lua: &Lua, kiki: &LuaTable) -> LuaResult<()> {
    let registry: Registry = Arc::default();
    let module = lua.create_table()?;

    let new_registry = Arc::clone(&registry);
    module.set(
        "new",
        lua.create_function(move |lua, (pattern, flags): (String, Option<String>)| {
            compile(lua, &new_registry, pattern, flags.unwrap_or_default())
        })?,
    )?;

    module.set(
        "escape",
        lua.create_function(|_, s: String| Ok(regex::escape(&s)))?,
    )?;

    let meta = lua.create_table()?;
    meta.set(
        "__call",
        lua.create_function(
            move |lua, (_, pattern, flags): (LuaTable, String, Option<String>)| {
                compile(lua, &registry, pattern, flags.unwrap_or_default())
            },
        )?,
    )?;
    module.set_metatable(Some(meta));

    kiki.set("regex", module)
}

/// Compile `pattern` with `flags`, or return the regex already compiled for them.
fn compile(lua: &Lua, registry: &Registry, pattern: String, flags: String) -> LuaResult<LuaRegex> {
    let key = (pattern, flags);
    if let Some(existing) = lookup(registry, &key) {
        return Ok(LuaRegex(existing));
    }

    if live_count(registry) >= MAX_LIVE_REGEXES {
        // Regexes that scripts have stopped using are only freed when Lua collects the
        // objects that hold them, so collect before giving up.
        lua.gc_collect()?;
        if live_count(registry) >= MAX_LIVE_REGEXES {
            return Err(LuaError::RuntimeError(format!(
                "kiki.regex: too many regexes (at most {MAX_LIVE_REGEXES} may be alive at once)"
            )));
        }
    }

    let regex = crate::scripting::regex::compile(&key.0, &key.1)
        .map_err(|e| LuaError::RuntimeError(format!("kiki.regex: {e}")))?;

    let compiled = Arc::new(Compiled {
        regex,
        pattern: key.0.clone(),
        flags: key.1.clone(),
        registry: Arc::clone(registry),
    });
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, Arc::downgrade(&compiled));
    Ok(LuaRegex(compiled))
}

/// The live regex compiled for `key`, if there is one.
fn lookup(registry: &Registry, key: &(String, String)) -> Option<Arc<Compiled>> {
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
        .and_then(Weak::upgrade)
}

/// How many regexes are alive in the VM.
fn live_count(registry: &Registry) -> usize {
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .filter(|w| w.strong_count() > 0)
        .count()
}

/// Convert a Lua `init` argument (1-based, negative counting from the end, as for
/// `string.find`) into a byte offset into a string of `len` bytes, or `None` if it starts
/// past the end, where nothing can match.
fn start_offset(len: usize, init: Option<i64>) -> Option<usize> {
    let start = match init.unwrap_or(1) {
        i if i > 0 => usize::try_from(i - 1).unwrap_or(usize::MAX),
        0 => 0,
        i => len.saturating_sub(usize::try_from(i.unsigned_abs()).unwrap_or(usize::MAX)),
    };
    (start <= len).then_some(start)
}

impl LuaUserData for LuaRegex {
    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("pattern", |_, this| Ok(this.0.pattern.clone()));
        fields.add_field_method_get("flags", |_, this| Ok(this.0.flags.clone()));
    }

    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("is_match", |_, this, s: LuaString| {
            Ok(this.0.regex.is_match(&s.as_bytes()))
        });

        methods.add_method("find", |_, this, (s, init): (LuaString, Option<i64>)| {
            let bytes = s.as_bytes();
            let found = start_offset(bytes.len(), init)
                .and_then(|start| this.0.regex.find_at(&bytes, start));
            Ok(match found {
                Some(m) => (Some(m.start() + 1), Some(m.end())),
                None => (None, None),
            })
        });

        methods.add_method("match", |lua, this, (s, init): (LuaString, Option<i64>)| {
            let bytes = s.as_bytes();
            start_offset(bytes.len(), init)
                .and_then(|start| this.0.regex.find_at(&bytes, start))
                .map(|m| lua.create_string(m.as_bytes()))
                .transpose()
        });

        methods.add_method(
            "captures",
            |lua, this, (s, init): (LuaString, Option<i64>)| {
                let bytes = s.as_bytes();
                let Some(caps) = start_offset(bytes.len(), init)
                    .and_then(|start| this.0.regex.captures_at(&bytes, start))
                else {
                    return Ok(None);
                };
                let table = lua.create_table()?;
                for (i, name) in this.0.regex.capture_names().enumerate() {
                    let value = match caps.get(i) {
                        Some(m) => LuaValue::String(lua.create_string(m.as_bytes())?),
                        None => LuaValue::Boolean(false),
                    };
                    if let Some(name) = name {
                        table.set(name, value.clone())?;
                    }
                    table.set(i, value)?;
                }
                Ok(Some(table))
            },
        );

        methods.add_method("match_all", |lua, this, s: LuaString| {
            let bytes = s.as_bytes();
            lua.create_sequence_from(
                this.0
                    .regex
                    .find_iter(&bytes)
                    .map(|m| lua.create_string(m.as_bytes()))
                    .collect::<LuaResult<Vec<_>>>()?,
            )
        });

        methods.add_method(
            "replace",
            |lua, this, (s, replacement, limit): (LuaString, LuaString, Option<usize>)| {
                let bytes = s.as_bytes();
                let replacement = replacement.as_bytes();
                lua.create_string(
                    this.0
                        .regex
                        .replacen(&bytes, limit.unwrap_or(0), &*replacement),
                )
            },
        );

        methods.add_method(
            "split",
            |lua, this, (s, limit): (LuaString, Option<usize>)| {
                let bytes = s.as_bytes();
                let parts: Vec<&[u8]> = match limit {
                    Some(n) => this.0.regex.splitn(&bytes, n).collect(),
                    None => this.0.regex.split(&bytes).collect(),
                };
                lua.create_sequence_from(
                    parts
                        .into_iter()
                        .map(|p| lua.create_string(p))
                        .collect::<LuaResult<Vec<_>>>()?,
                )
            },
        );

        methods.add_meta_method(LuaMetaMethod::ToString, |_, this, ()| {
            Ok(format!("kiki.regex({:?})", this.0.pattern))
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn lua() -> Lua {
        let lua = Lua::new();
        let kiki = lua.create_table().unwrap();
        install(&lua, &kiki).unwrap();
        lua.globals().set("kiki", kiki).unwrap();
        lua
    }

    fn eval<T: FromLuaMulti>(lua: &Lua, code: &str) -> LuaResult<T> {
        lua.load(code).eval()
    }

    #[test]
    fn is_match_supports_alternation_and_flags() {
        let lua = lua();
        let (a, b, c): (bool, bool, bool) = eval(
            &lua,
            r#"
            local re = kiki.regex([[\b(sponsored|giveaway)\b]], "i")
            return re:is_match("A SPONSORED post"), re:is_match("a giveaway"), re:is_match("unsponsored")
            "#,
        )
        .unwrap();
        assert!(a && b && !c);
    }

    #[test]
    fn new_is_the_same_as_calling_the_module() {
        let lua = lua();
        let ok: bool = eval(&lua, r#"return kiki.regex.new("a+"):is_match("caat")"#).unwrap();
        assert!(ok);
    }

    #[test]
    fn find_returns_one_based_inclusive_positions() {
        let lua = lua();
        let (start, end): (Option<i64>, Option<i64>) =
            eval(&lua, r#"return kiki.regex("b+"):find("abbbc")"#).unwrap();
        assert_eq!((start, end), (Some(2), Some(4)));

        let (start, end): (Option<i64>, Option<i64>) =
            eval(&lua, r#"return kiki.regex("b"):find("abcb", 3)"#).unwrap();
        assert_eq!((start, end), (Some(4), Some(4)));

        let (start, end): (Option<i64>, Option<i64>) =
            eval(&lua, r#"return kiki.regex("b"):find("abcb", -1)"#).unwrap();
        assert_eq!((start, end), (Some(4), Some(4)));

        let start: Option<i64> = eval(&lua, r#"return kiki.regex("z"):find("abc")"#).unwrap();
        assert_eq!(start, None);

        let start: Option<i64> = eval(&lua, r#"return kiki.regex("a"):find("abc", 10)"#).unwrap();
        assert_eq!(start, None);
    }

    #[test]
    fn match_returns_the_matched_text() {
        let lua = lua();
        let m: Option<String> =
            eval(&lua, r#"return kiki.regex("[0-9]+"):match("v12.3")"#).unwrap();
        assert_eq!(m.as_deref(), Some("12"));
        let m: Option<String> = eval(&lua, r#"return kiki.regex("[0-9]+"):match("none")"#).unwrap();
        assert_eq!(m, None);
    }

    #[test]
    fn captures_are_indexed_by_number_and_name() {
        let lua = lua();
        let (whole, year, month, missing): (String, String, String, bool) = eval(
            &lua,
            r#"
            local c = kiki.regex([[(?P<year>\d{4})-(?P<month>\d{2})(x)?]]):captures("on 2026-09-28")
            return c[0], c.year, c[2], c[3]
            "#,
        )
        .unwrap();
        assert_eq!(whole, "2026-09");
        assert_eq!(year, "2026");
        assert_eq!(month, "09");
        assert!(!missing, "a group that did not take part is false");

        let none: Option<LuaTable> =
            eval(&lua, r#"return kiki.regex("(x)"):captures("abc")"#).unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn match_all_returns_every_match() {
        let lua = lua();
        let all: Vec<String> = eval(
            &lua,
            r#"return kiki.regex("[a-z]+"):match_all("one, two; three")"#,
        )
        .unwrap();
        assert_eq!(all, ["one", "two", "three"]);
    }

    #[test]
    fn replace_expands_groups_and_honours_limit() {
        let lua = lua();
        let s: String = eval(
            &lua,
            r#"return kiki.regex([[(?P<w>\w+)@example\.com]]):replace("a@example.com b@example.com", "<$w>")"#,
        )
        .unwrap();
        assert_eq!(s, "<a> <b>");
        let s: String = eval(&lua, r#"return kiki.regex("o"):replace("foo", "0", 1)"#).unwrap();
        assert_eq!(s, "f0o");
    }

    #[test]
    fn split_splits_on_matches() {
        let lua = lua();
        let parts: Vec<String> =
            eval(&lua, r#"return kiki.regex([[\s*,\s*]]):split("a , b,c")"#).unwrap();
        assert_eq!(parts, ["a", "b", "c"]);
        let parts: Vec<String> = eval(&lua, r#"return kiki.regex(","):split("a,b,c", 2)"#).unwrap();
        assert_eq!(parts, ["a", "b,c"]);
    }

    #[test]
    fn escape_quotes_metacharacters() {
        let lua = lua();
        let ok: bool = eval(
            &lua,
            r#"return kiki.regex(kiki.regex.escape("a.b*c")):is_match("xa.b*cx")"#,
        )
        .unwrap();
        assert!(ok);
    }

    #[test]
    fn pattern_and_flags_are_readable() {
        let lua = lua();
        let (pattern, flags, shown): (String, String, String) = eval(
            &lua,
            r#"local re = kiki.regex("a+", "i") return re.pattern, re.flags, tostring(re)"#,
        )
        .unwrap();
        assert_eq!(pattern, "a+");
        assert_eq!(flags, "i");
        assert_eq!(shown, r#"kiki.regex("a+")"#);
    }

    #[test]
    fn works_on_bytes_that_are_not_utf8() {
        let lua = lua();
        let ok: bool = eval(&lua, r#"return kiki.regex("b"):is_match("\xffb")"#).unwrap();
        assert!(ok);
    }

    #[test]
    fn invalid_patterns_and_flags_are_errors() {
        let lua = lua();
        let err = eval::<LuaValue>(&lua, r#"return kiki.regex("(")"#).unwrap_err();
        assert!(err.to_string().contains("invalid pattern"), "{err}");
        let err = eval::<LuaValue>(&lua, r#"return kiki.regex("a", "q")"#).unwrap_err();
        assert!(err.to_string().contains("unknown flag"), "{err}");
    }

    #[test]
    fn a_long_list_of_words_fits_within_the_limits() {
        let lua = lua();
        let words: Vec<String> = (0..200).map(|i| format!("keyword{i}")).collect();
        let ok: bool = eval(
            &lua,
            &format!(
                r#"return kiki.regex([[\b({})\b]], "i"):is_match("a KEYWORD199 here")"#,
                words.join("|")
            ),
        )
        .unwrap();
        assert!(ok);
    }

    #[test]
    fn oversized_patterns_are_rejected() {
        let lua = lua();
        let err = eval::<LuaValue>(&lua, r#"return kiki.regex([[\w{1000}{1000}]])"#).unwrap_err();
        assert!(err.to_string().contains("invalid pattern"), "{err}");
    }

    #[test]
    fn the_same_pattern_is_compiled_once() {
        // Every object shares one compiled regex, so keeping many of them alive never
        // reaches the limit.
        let lua = lua();
        eval::<()>(
            &lua,
            &format!(
                r#"
                keep = {{}}
                for i = 1, {} do keep[i] = kiki.regex("same") end
                "#,
                MAX_LIVE_REGEXES * 4
            ),
        )
        .unwrap();
    }

    #[test]
    fn too_many_live_regexes_is_an_error() {
        let lua = lua();
        let err = eval::<()>(
            &lua,
            &format!(
                r#"
                keep = {{}}
                for i = 1, {} do keep[i] = kiki.regex("a" .. i) end
                "#,
                MAX_LIVE_REGEXES + 1
            ),
        )
        .unwrap_err();
        assert!(err.to_string().contains("too many regexes"), "{err}");
    }

    #[test]
    fn regexes_no_longer_referenced_are_freed() {
        let lua = lua();
        eval::<()>(
            &lua,
            &format!(
                r#"for i = 1, {} do kiki.regex("a" .. i):is_match("a") end"#,
                MAX_LIVE_REGEXES * 4
            ),
        )
        .unwrap();
    }
}
