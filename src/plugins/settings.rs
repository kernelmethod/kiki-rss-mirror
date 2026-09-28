//! Typed descriptions of a plugin's settings.
//!
//! A manifest may describe the settings of its `[config]` table with a
//! `[[settings]]` array. Each entry names a config key, says what type of
//! value it holds, and gives it a label and description for people editing
//! it. The web UI uses these to show a form field that fits each setting,
//! rather than asking for JSON, and the config API and `kiki plugin config
//! set` refuse values of the wrong type.
//!
//! ```toml
//! [config]
//! limit = 10
//! mode = "fast"
//!
//! [[settings]]
//! name = "limit"
//! type = "integer"
//! label = "Limit"
//! description = "How many entries to look at."
//! min = 1
//!
//! [[settings]]
//! name = "mode"
//! type = "choice"
//! choices = ["fast", "thorough"]
//! ```
//!
//! The types are:
//!
//! | `type`    | Value                                  | Options                          |
//! |-----------|----------------------------------------|----------------------------------|
//! | `string`  | A string                               | `multiline`                      |
//! | `integer` | A whole number                         | `min`, `max`                     |
//! | `number`  | Any number                             | `min`, `max`                     |
//! | `boolean` | `true` or `false`                      |                                  |
//! | `choice`  | One of a fixed set of strings          | `choices` (required)             |
//! | `list`    | A list of values of one type           | `items` (required): a type       |
//! | `object`  | A table with named fields              | `fields` (required): settings    |
//! | `json`    | Any value; edited as JSON              |                                  |
//!
//! `items` is a table holding a `type` and that type's options, such as
//! `{ type = "choice", choices = ["a", "b"] }`. `fields` is an array of
//! settings, written like the top-level ones; a field with `required =
//! true` must be present in every object. Object field names are made of
//! letters, digits, `_` and `-`.
//!
//! Settings are optional: config keys without one are shown as JSON, and
//! a setting's default, if it has one, is its value in `[config]`, which
//! must be of the setting's type. A top-level setting may still be
//! overridden with `null`, which hides its default from the plugin.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fmt;

/// Deepest nesting of lists and objects a setting may describe.
pub const MAX_SETTING_DEPTH: usize = 8;

/// A setting of a plugin: a key in its config, or a field of an object
/// setting, and the type of value it holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Setting {
    /// The config key, or object field, the setting describes.
    pub name: String,

    /// A short, human-readable name for the setting. Defaults to `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// What the setting does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// For a field of an object, whether every object must have it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,

    /// The type of value the setting holds.
    #[serde(flatten)]
    pub kind: SettingType,
}

impl Setting {
    /// The setting's label, or its name if it has none.
    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
}

/// The type of value a [`Setting`] holds, and the constraints on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SettingType {
    /// A string.
    String {
        /// Whether the string may span several lines.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        multiline: bool,
    },
    /// A whole number.
    Integer {
        /// The smallest allowed value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<i64>,
        /// The largest allowed value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<i64>,
    },
    /// Any finite number.
    Number {
        /// The smallest allowed value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        /// The largest allowed value.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
    },
    /// `true` or `false`.
    Boolean,
    /// One of a fixed set of strings.
    Choice {
        /// The allowed strings.
        choices: Vec<String>,
    },
    /// A list whose items are all of one type.
    List {
        /// The type of the list's items.
        items: Box<SettingType>,
    },
    /// An object with named fields.
    Object {
        /// The object's fields. Objects may not have fields other than
        /// these.
        fields: Vec<Setting>,
    },
    /// Any value.
    Json,
}

/// Where in a config a value is, for error messages: `rules[2].pattern`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValuePath(String);

impl ValuePath {
    /// The path of top-level config key `key`.
    pub fn key(key: &str) -> Self {
        Self(key.to_owned())
    }

    /// The path of field `name` of the object at this path.
    pub fn field(&self, name: &str) -> Self {
        Self(format!("{}.{name}", self.0))
    }

    /// The path of item `index` of the list at this path.
    pub fn index(&self, index: usize) -> Self {
        Self(format!("{}[{index}]", self.0))
    }
}

impl fmt::Display for ValuePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A config value that does not match its setting.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{path}: {message}")]
pub struct InvalidValue {
    /// Where the value is.
    pub path: ValuePath,
    /// What is wrong with it.
    pub message: String,
}

fn invalid(path: &ValuePath, message: impl Into<String>) -> InvalidValue {
    InvalidValue {
        path: path.clone(),
        message: message.into(),
    }
}

impl SettingType {
    /// A short description of the type, for error messages: "an integer".
    pub fn describe(&self) -> String {
        match self {
            Self::String { .. } => "a string".into(),
            Self::Integer { .. } => "an integer".into(),
            Self::Number { .. } => "a number".into(),
            Self::Boolean => "true or false".into(),
            Self::Choice { choices } => format!("one of {}", quoted_list(choices)),
            Self::List { .. } => "a list".into(),
            Self::Object { .. } => "a table".into(),
            Self::Json => "any value".into(),
        }
    }

    /// Checks that `value`, found at `path`, is of this type.
    ///
    /// Inside an object, a field set to `null` counts as missing.
    ///
    /// # Errors
    ///
    /// Returns an [`InvalidValue`] for the first part of `value` that does
    /// not match.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::plugins::settings::{SettingType, ValuePath};
    /// use serde_json::json;
    ///
    /// let kind = SettingType::Integer { min: Some(1), max: None };
    /// assert!(kind.check(&json!(3), &ValuePath::key("n")).is_ok());
    /// assert!(kind.check(&json!(0), &ValuePath::key("n")).is_err());
    /// assert!(kind.check(&json!("3"), &ValuePath::key("n")).is_err());
    /// ```
    pub fn check(&self, value: &Value, path: &ValuePath) -> Result<(), InvalidValue> {
        let wrong_type = || invalid(path, format!("expected {}", self.describe()));
        match self {
            Self::String { multiline } => {
                let s = value.as_str().ok_or_else(wrong_type)?;
                if !multiline && s.contains('\n') {
                    return Err(invalid(path, "must be a single line"));
                }
            }
            Self::Integer { min, max } => {
                let n = value.as_i64().ok_or_else(wrong_type)?;
                check_range(n, *min, *max, path)?;
            }
            Self::Number { min, max } => {
                let n = value.as_f64().ok_or_else(wrong_type)?;
                check_range(n, *min, *max, path)?;
            }
            Self::Boolean => {
                value.as_bool().ok_or_else(wrong_type)?;
            }
            Self::Choice { choices } => {
                let s = value.as_str().ok_or_else(wrong_type)?;
                if !choices.iter().any(|c| c == s) {
                    return Err(wrong_type());
                }
            }
            Self::List { items } => {
                let list = value.as_array().ok_or_else(wrong_type)?;
                for (i, item) in list.iter().enumerate() {
                    items.check(item, &path.index(i))?;
                }
            }
            Self::Object { fields } => {
                let object = value.as_object().ok_or_else(wrong_type)?;
                check_object(fields, object, path)?;
            }
            Self::Json => {}
        }
        Ok(())
    }

    /// Checks that this type is well formed, and that the settings inside
    /// it are, `depth` levels down from the top of the config.
    fn validate(&self, path: &ValuePath, depth: usize) -> Result<(), InvalidSettings> {
        let bad = |message: String| InvalidSettings {
            path: path.clone(),
            message,
        };
        if depth > MAX_SETTING_DEPTH {
            return Err(bad(format!(
                "settings may be nested at most {MAX_SETTING_DEPTH} levels deep"
            )));
        }
        match self {
            Self::Integer {
                min: Some(min),
                max: Some(max),
            } if min > max => return Err(bad("min is larger than max".into())),
            Self::Number { min, max } => {
                if min.is_some_and(|n| !n.is_finite()) || max.is_some_and(|n| !n.is_finite()) {
                    return Err(bad("min and max must be finite".into()));
                }
                if let (Some(min), Some(max)) = (min, max) {
                    if min > max {
                        return Err(bad("min is larger than max".into()));
                    }
                }
            }
            Self::Choice { choices } => {
                if choices.is_empty() {
                    return Err(bad("a choice must have at least one choice".into()));
                }
                let mut seen = HashSet::new();
                if let Some(dup) = choices.iter().find(|c| !seen.insert(c.as_str())) {
                    return Err(bad(format!("choice {dup:?} is listed twice")));
                }
            }
            Self::List { items } => items.validate(&path.index(0), depth + 1)?,
            Self::Object { fields } => validate_fields(fields, path, depth + 1, true)?,
            _ => {}
        }
        Ok(())
    }
}

fn check_range<T: PartialOrd + fmt::Display>(
    n: T,
    min: Option<T>,
    max: Option<T>,
    path: &ValuePath,
) -> Result<(), InvalidValue> {
    if let Some(min) = min.filter(|min| n < *min) {
        return Err(invalid(path, format!("must be at least {min}")));
    }
    if let Some(max) = max.filter(|max| n > *max) {
        return Err(invalid(path, format!("must be at most {max}")));
    }
    Ok(())
}

fn check_object(
    fields: &[Setting],
    object: &Map<String, Value>,
    path: &ValuePath,
) -> Result<(), InvalidValue> {
    if let Some(unknown) = object
        .keys()
        .find(|k| !fields.iter().any(|f| &f.name == *k))
    {
        return Err(invalid(
            path,
            format!(
                "unknown field {unknown:?}; expected {}",
                quoted_list(fields.iter().map(|f| &f.name))
            ),
        ));
    }
    for field in fields {
        match object.get(&field.name).filter(|v| !v.is_null()) {
            Some(value) => field.kind.check(value, &path.field(&field.name))?,
            None if field.required => {
                return Err(invalid(&path.field(&field.name), "is required"));
            }
            None => {}
        }
    }
    Ok(())
}

fn quoted_list<T: AsRef<str>>(items: impl IntoIterator<Item = T>) -> String {
    items
        .into_iter()
        .map(|s| format!("{:?}", s.as_ref()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A manifest's `[[settings]]` that are not well formed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{path}: {message}")]
pub struct InvalidSettings {
    /// The setting that is not well formed.
    pub path: ValuePath,
    /// What is wrong with it.
    pub message: String,
}

fn validate_fields(
    fields: &[Setting],
    parent: &ValuePath,
    depth: usize,
    nested: bool,
) -> Result<(), InvalidSettings> {
    let mut names = HashSet::new();
    for field in fields {
        let path = if nested {
            parent.field(&field.name)
        } else {
            ValuePath::key(&field.name)
        };
        let bad = |message: &str| InvalidSettings {
            path: path.clone(),
            message: message.into(),
        };
        if field.name.is_empty() {
            return Err(bad("a setting must have a name"));
        }
        if nested
            && !field
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(bad(
                "field names may only hold ASCII letters, digits, '_' and '-'",
            ));
        }
        if !names.insert(field.name.as_str()) {
            return Err(bad("is described twice"));
        }
        field.kind.validate(&path, depth)?;
    }
    Ok(())
}

/// Checks that a manifest's `settings` are well formed, and that each of
/// the defaults in `config` that a setting describes matches it.
///
/// # Errors
///
/// Returns an [`InvalidSettings`] naming the first setting that is not well
/// formed, or whose default does not match it.
pub fn validate_settings(
    settings: &[Setting],
    config: &Map<String, Value>,
) -> Result<(), InvalidSettings> {
    validate_fields(settings, &ValuePath::default(), 0, false)?;
    check_config(settings, config).map_err(|e| InvalidSettings {
        path: e.path,
        message: format!("the default in [config] does not match: {}", e.message),
    })
}

/// Checks each value of `config` that one of `settings` describes against
/// it. Keys no setting describes, and keys set to `null`, are not checked.
///
/// # Errors
///
/// Returns an [`InvalidValue`] for the first value that does not match.
///
/// # Examples
///
/// ```
/// use kiki_rss::plugins::settings::{check_config, Setting, SettingType};
/// use serde_json::json;
///
/// let settings = [Setting {
///     name: "on".into(),
///     label: None,
///     description: None,
///     required: false,
///     kind: SettingType::Boolean,
/// }];
/// let ok = json!({"on": true, "other": 1});
/// assert!(check_config(&settings, ok.as_object().unwrap()).is_ok());
/// let bad = json!({"on": "yes"});
/// assert_eq!(
///     check_config(&settings, bad.as_object().unwrap()).unwrap_err().to_string(),
///     "on: expected true or false",
/// );
/// ```
pub fn check_config(settings: &[Setting], config: &Map<String, Value>) -> Result<(), InvalidValue> {
    for setting in settings {
        if let Some(value) = config.get(&setting.name).filter(|v| !v.is_null()) {
            setting.kind.check(value, &ValuePath::key(&setting.name))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(text: &str) -> Vec<Setting> {
        #[derive(Deserialize)]
        struct Doc {
            settings: Vec<Setting>,
        }
        toml::from_str::<Doc>(text).unwrap().settings
    }

    fn rules() -> Vec<Setting> {
        parse(
            r#"
            [[settings]]
            name = "rules"
            type = "list"
            [settings.items]
            type = "object"
            [[settings.items.fields]]
            name = "pattern"
            type = "string"
            required = true
            [[settings.items.fields]]
            name = "fields"
            type = "list"
            items = { type = "choice", choices = ["title", "content"] }
            [[settings.items.fields]]
            name = "weight"
            type = "number"
            min = 0
            "#,
        )
    }

    #[test]
    fn settings_parse_from_toml() {
        let settings = rules();
        assert_eq!(settings.len(), 1);
        let SettingType::List { items } = &settings[0].kind else {
            panic!("expected a list");
        };
        let SettingType::Object { fields } = items.as_ref() else {
            panic!("expected an object");
        };
        assert_eq!(fields[0].name, "pattern");
        assert!(fields[0].required);
        assert_eq!(
            fields[2].kind,
            SettingType::Number {
                min: Some(0.0),
                max: None
            }
        );
    }

    #[test]
    fn settings_round_trip_through_json_and_toml() {
        let settings = rules();
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<Setting>>(&json).unwrap(),
            settings
        );

        #[derive(Serialize, Deserialize)]
        struct Doc {
            settings: Vec<Setting>,
        }
        let text = toml::to_string_pretty(&Doc {
            settings: settings.clone(),
        })
        .unwrap();
        assert_eq!(toml::from_str::<Doc>(&text).unwrap().settings, settings);
    }

    #[test]
    fn check_accepts_matching_values() {
        let settings = rules();
        let config = json!({
            "rules": [
                {"pattern": "a", "fields": ["title"], "weight": 1.5},
                {"pattern": "b", "weight": null},
            ],
        });
        check_config(&settings, config.as_object().unwrap()).unwrap();
        check_config(&settings, json!({"rules": null}).as_object().unwrap()).unwrap();
    }

    #[test]
    fn check_rejects_mismatches() {
        let settings = rules();
        for (config, error) in [
            (json!({"rules": {}}), "rules: expected a list"),
            (json!({"rules": [{}]}), "rules[0].pattern: is required"),
            (
                json!({"rules": [{"pattern": "a", "fields": ["url"]}]}),
                "rules[0].fields[0]: expected one of \"title\", \"content\"",
            ),
            (
                json!({"rules": [{"pattern": "a", "weight": -1}]}),
                "rules[0].weight: must be at least 0",
            ),
            (
                json!({"rules": [{"pattern": "a", "extra": 1}]}),
                "rules[0]: unknown field \"extra\"; expected \"pattern\", \"fields\", \"weight\"",
            ),
        ] {
            assert_eq!(
                check_config(&settings, config.as_object().unwrap())
                    .unwrap_err()
                    .to_string(),
                error
            );
        }
    }

    #[test]
    fn strings_are_single_line_unless_multiline() {
        let path = ValuePath::key("s");
        let line = SettingType::String { multiline: false };
        assert!(line.check(&json!("a\nb"), &path).is_err());
        let text = SettingType::String { multiline: true };
        assert!(text.check(&json!("a\nb"), &path).is_ok());
    }

    #[test]
    fn integers_must_be_whole_and_in_range() {
        let path = ValuePath::key("n");
        let kind = SettingType::Integer {
            min: Some(0),
            max: Some(10),
        };
        assert!(kind.check(&json!(10), &path).is_ok());
        assert!(kind.check(&json!(1.5), &path).is_err());
        assert_eq!(
            kind.check(&json!(11), &path).unwrap_err().to_string(),
            "n: must be at most 10"
        );
    }

    #[test]
    fn validate_rejects_malformed_settings() {
        for (text, error) in [
            (
                "[[settings]]\nname = 'a'\ntype = 'choice'\nchoices = []",
                "a: a choice must have at least one choice",
            ),
            (
                "[[settings]]\nname = 'a'\ntype = 'integer'\nmin = 2\nmax = 1",
                "a: min is larger than max",
            ),
            (
                "[[settings]]\nname = 'a'\ntype = 'boolean'\n[[settings]]\nname = 'a'\ntype = 'json'",
                "a: is described twice",
            ),
            (
                "[[settings]]\nname = 'a'\ntype = 'object'\n[[settings.fields]]\nname = 'b.c'\ntype = 'json'",
                "a.b.c: field names may only hold ASCII letters, digits, '_' and '-'",
            ),
        ] {
            assert_eq!(
                validate_settings(&parse(text), &Map::new())
                    .unwrap_err()
                    .to_string(),
                error,
                "{text}"
            );
        }
    }

    #[test]
    fn validate_checks_defaults() {
        let settings = parse("[[settings]]\nname = 'a'\ntype = 'integer'");
        assert!(validate_settings(&settings, json!({"a": 1}).as_object().unwrap()).is_ok());
        assert_eq!(
            validate_settings(&settings, json!({"a": "x"}).as_object().unwrap())
                .unwrap_err()
                .to_string(),
            "a: the default in [config] does not match: expected an integer"
        );
    }

    #[test]
    fn validate_limits_nesting() {
        let mut kind = SettingType::Json;
        for _ in 0..=MAX_SETTING_DEPTH {
            kind = SettingType::List {
                items: Box::new(kind),
            };
        }
        let settings = [Setting {
            name: "deep".into(),
            label: None,
            description: None,
            required: false,
            kind,
        }];
        assert!(validate_settings(&settings, &Map::new()).is_err());
    }
}
