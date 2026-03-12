//! Lua-based [`ScriptRunner`] implementation.
//!
//! This module is compiled only when the `lua` feature is enabled.

use super::{FeedEntry, ScriptRunner};
use mlua::prelude::*;
use thiserror::Error;
use tracing::warn;

/// Errors that can occur during script loading or execution.
#[derive(Debug, Error)]
pub enum ScriptError {
    /// A script failed to compile or did not return a function.
    #[error("script load error: {0}")]
    ScriptLoadError(#[source] LuaError),

    /// A runtime error occurred while executing a Lua script.
    #[error("script execution error: {0}")]
    ScriptExecutionError(#[source] LuaError),

    /// A script returned something other than `nil` or a table.
    #[error("script returned an invalid type: expected table or nil, got {0}")]
    InvalidReturnType(String),
}

impl IntoLua for FeedEntry {
    fn into_lua(self, lua: &Lua) -> LuaResult<LuaValue> {
        let table = lua.create_table()?;
        table.set("feed_id", self.feed_id)?;
        table.set("syndication_format", self.syndication_format)?;
        table.set("guid", self.guid)?;
        table.set("published_at", self.published_at)?;
        table.set("title", self.title)?;
        table.set("url", self.url)?;
        table.set("content", self.content)?;

        let tags = lua.create_table()?;
        for (i, tag) in self.tags.iter().enumerate() {
            tags.set(i + 1, tag.as_str())?;
        }
        table.set("tags", tags)?;

        Ok(LuaValue::Table(table))
    }
}

impl FromLua for FeedEntry {
    fn from_lua(value: LuaValue, _lua: &Lua) -> LuaResult<Self> {
        let table = match value {
            LuaValue::Table(t) => t,
            other => {
                return Err(LuaError::FromLuaConversionError {
                    from: other.type_name(),
                    to: "FeedEntry".to_string(),
                    message: Some("expected a table".to_string()),
                })
            }
        };

        // Identity fields: read from the table as-is (modifications are ignored later in
        // process_entry, which preserves the originals).
        let feed_id: i64 = table.get("feed_id")?;
        let syndication_format: String = table.get("syndication_format")?;
        let guid: String = table.get("guid")?;

        let published_at: Option<i64> = table.get("published_at")?;
        let title: String = table.get("title")?;
        let url: Option<String> = table.get("url")?;
        let content: Option<String> = table.get("content")?;

        let tags_table: LuaTable = table.get("tags")?;
        let mut tags = Vec::new();
        for pair in tags_table.sequence_values::<String>() {
            tags.push(pair?);
        }

        Ok(FeedEntry {
            feed_id,
            syndication_format,
            guid,
            published_at,
            title,
            url,
            content,
            tags,
        })
    }
}

/// Runs a sequence of Lua scripts against feed entries.
///
/// # Sandboxing
///
/// The Lua VM is initialised with a restricted standard library (`string`, `table`, `math`,
/// plus safe `os` functions). Dangerous globals (`os.execute`, `io`, `require`, `dofile`,
/// `loadfile`, `debug`) are removed before any user script is loaded.
///
/// # Script loading
///
/// Each script source is evaluated as a Lua chunk that must **return a function**. The
/// function is stored at construction time; no re-compilation happens per entry.
pub struct LuaScriptRunner {
    lua: Lua,
    scripts: Vec<LuaFunction>,
}

impl LuaScriptRunner {
    /// Creates a new [`LuaScriptRunner`] from a slice of Lua script source strings.
    ///
    /// Each source must be a Lua chunk that returns a function. The VM is sandboxed before
    /// any script is loaded.
    ///
    /// # Errors
    ///
    /// Returns [`ScriptError::ScriptLoadError`] if any script fails to compile or does not
    /// return a function.
    pub fn new(script_sources: &[String]) -> Result<Self, ScriptError> {
        let lua = Lua::new_with(
            LuaStdLib::STRING | LuaStdLib::TABLE | LuaStdLib::MATH | LuaStdLib::OS,
            LuaOptions::default(),
        )
        .map_err(ScriptError::ScriptLoadError)?;

        // Remove dangerous OS functions, leaving only os.time, os.clock, os.date, os.difftime.
        {
            let os: LuaTable = lua
                .globals()
                .get("os")
                .map_err(ScriptError::ScriptLoadError)?;
            for func in &["execute", "exit", "getenv", "remove", "rename", "tmpname"] {
                os.set(*func, LuaValue::Nil)
                    .map_err(ScriptError::ScriptLoadError)?;
            }
        }

        // Remove other dangerous globals.
        for global in &["require", "dofile", "loadfile", "debug", "io", "package"] {
            lua.globals()
                .set(*global, LuaValue::Nil)
                .map_err(ScriptError::ScriptLoadError)?;
        }

        let mut scripts = Vec::with_capacity(script_sources.len());
        for source in script_sources {
            let func: LuaFunction = lua
                .load(source.as_str())
                .eval()
                .map_err(ScriptError::ScriptLoadError)?;
            scripts.push(func);
        }

        Ok(Self { lua, scripts })
    }

    /// Passes `entry` through each script in sequence, returning `Err` on `ScriptError`.
    ///
    /// Used internally; external callers go through the [`ScriptRunner`] trait which maps
    /// errors to [`anyhow::Error`].
    fn run_scripts(&self, entry: FeedEntry) -> Result<Option<FeedEntry>, ScriptError> {
        // Preserve identity fields so scripts cannot corrupt them.
        let feed_id = entry.feed_id;
        let syndication_format = entry.syndication_format.clone();
        let guid = entry.guid.clone();

        let mut current = entry;

        for script in &self.scripts {
            let lua_entry = current
                .clone()
                .into_lua(&self.lua)
                .map_err(ScriptError::ScriptExecutionError)?;

            let result: LuaValue = match script.call(lua_entry) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "lua script execution error; passing entry through unmodified");
                    continue;
                }
            };

            match result {
                LuaValue::Nil => return Ok(None),
                LuaValue::Table(_) => {
                    let mut modified = match FeedEntry::from_lua(result, &self.lua) {
                        Ok(e) => e,
                        Err(e) => {
                            warn!(
                                error = %ScriptError::ScriptExecutionError(e),
                                "failed to convert lua table back to FeedEntry; passing entry through unmodified"
                            );
                            continue;
                        }
                    };
                    // Restore identity fields.
                    modified.feed_id = feed_id;
                    modified.syndication_format = syndication_format.clone();
                    modified.guid = guid.clone();
                    current = modified;
                }
                other => {
                    let type_name = other.type_name().to_string();
                    warn!(
                        return_type = %type_name,
                        "lua script returned an invalid type; passing entry through unmodified"
                    );
                }
            }
        }

        Ok(Some(current))
    }
}

impl ScriptRunner for LuaScriptRunner {
    /// Passes `entry` through each configured Lua script in sequence.
    ///
    /// See [`ScriptRunner::process_entry`] for the full contract. Script execution errors are
    /// logged as warnings and cause the entry to pass through unmodified; only VM-level
    /// failures propagate as `Err`.
    fn process_entry(&self, entry: FeedEntry) -> anyhow::Result<Option<FeedEntry>> {
        self.run_scripts(entry)
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry() -> FeedEntry {
        FeedEntry {
            feed_id: 1,
            syndication_format: "rss".to_string(),
            guid: "test-guid".to_string(),
            published_at: Some(1_700_000_000),
            title: "Test Title".to_string(),
            url: Some("https://example.com".to_string()),
            content: Some("<p>Hello</p>".to_string()),
            tags: vec![],
        }
    }

    #[test]
    fn passthrough_script_returns_entry_unmodified() {
        let runner =
            LuaScriptRunner::new(&["return function(entry) return entry end".to_string()]).unwrap();
        let entry = make_entry();
        let result = runner.process_entry(entry.clone()).unwrap();
        let out = result.unwrap();
        assert_eq!(out.title, entry.title);
        assert_eq!(out.guid, entry.guid);
        assert_eq!(out.feed_id, entry.feed_id);
    }

    #[test]
    fn filtering_script_returns_none() {
        let runner =
            LuaScriptRunner::new(&["return function(entry) return nil end".to_string()]).unwrap();
        let result = runner.process_entry(make_entry()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn modifying_script_changes_title() {
        let runner = LuaScriptRunner::new(&[
            r#"return function(entry) entry.title = "[NEWS] " .. entry.title; return entry end"#
                .to_string(),
        ])
        .unwrap();
        let result = runner.process_entry(make_entry()).unwrap().unwrap();
        assert_eq!(result.title, "[NEWS] Test Title");
    }

    #[test]
    fn script_chaining_runs_in_order() {
        let runner = LuaScriptRunner::new(&[
            r#"return function(entry) entry.title = "A:" .. entry.title; return entry end"#
                .to_string(),
            r#"return function(entry) entry.title = "B:" .. entry.title; return entry end"#
                .to_string(),
        ])
        .unwrap();
        let result = runner.process_entry(make_entry()).unwrap().unwrap();
        assert_eq!(result.title, "B:A:Test Title");
    }

    #[test]
    fn tagging_script_populates_tags() {
        let runner = LuaScriptRunner::new(&[
            r#"return function(entry) table.insert(entry.tags, "rust"); return entry end"#
                .to_string(),
        ])
        .unwrap();
        let result = runner.process_entry(make_entry()).unwrap().unwrap();
        assert_eq!(result.tags, vec!["rust"]);
    }

    #[test]
    fn tag_removal_works() {
        let mut entry = make_entry();
        entry.tags = vec!["keep".to_string(), "remove".to_string()];
        let runner = LuaScriptRunner::new(&[r#"
            return function(entry)
                local filtered = {}
                for _, tag in ipairs(entry.tags) do
                    if tag ~= "remove" then
                        table.insert(filtered, tag)
                    end
                end
                entry.tags = filtered
                return entry
            end
        "#
        .to_string()])
        .unwrap();
        let result = runner.process_entry(entry).unwrap().unwrap();
        assert_eq!(result.tags, vec!["keep"]);
    }

    #[test]
    fn erroring_script_does_not_filter_entry() {
        let runner =
            LuaScriptRunner::new(&["return function(entry) error('oops') end".to_string()])
                .unwrap();
        let result = runner.process_entry(make_entry()).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn identity_fields_cannot_be_changed_by_script() {
        let runner = LuaScriptRunner::new(&[r#"
            return function(entry)
                entry.feed_id = 999
                entry.guid = "hacked"
                entry.syndication_format = "atom"
                return entry
            end
        "#
        .to_string()])
        .unwrap();
        let result = runner.process_entry(make_entry()).unwrap().unwrap();
        assert_eq!(result.feed_id, 1);
        assert_eq!(result.guid, "test-guid");
        assert_eq!(result.syndication_format, "rss");
    }

    #[test]
    fn first_nil_in_chain_stops_processing() {
        let runner = LuaScriptRunner::new(&[
            "return function(entry) return nil end".to_string(),
            // This script would panic if called, but it should never be reached.
            r#"return function(entry) error("should not run") end"#.to_string(),
        ])
        .unwrap();
        let result = runner.process_entry(make_entry()).unwrap();
        assert!(result.is_none());
    }
}
