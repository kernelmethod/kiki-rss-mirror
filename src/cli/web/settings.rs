//! Form fields for plugin settings.
//!
//! A plugin's manifest can describe each setting in its config with a
//! [`Setting`]. For those, the plugin's page shows form fields that fit the
//! setting, built by [`render_input`], rather than asking for JSON, and
//! [`parse_input`] turns what the browser submits back into a config value.
//!
//! Pages may not run scripts, so every field is plain HTML. Fields are
//! named after where their value goes, starting from [`VALUE_FIELD`]:
//! `v` is the value itself, `v.pattern` a field of an object, and
//! `v.2.pattern` a field of the third object in a list. Object field names
//! hold no `.` or `#` (see [`crate::plugins::settings`]), so these names
//! never clash. A list of objects is shown as one fieldset per item, with a
//! box to tick to remove it, and a blank one to fill in to add an item.
//! `v#count` says how many fieldsets were shown, and `v.2#present` marks
//! the fieldsets of items already in the list, which are kept even if all
//! their fields are left empty.

use crate::plugins::settings::{InvalidValue, Setting, SettingType, ValuePath};
use quick_xml::escape::escape;
use serde_json::{Map, Number, Value};
use std::collections::{HashMap, HashSet};

/// The name of the form field holding a setting's value, and the prefix of
/// the names of the fields holding its parts.
pub const VALUE_FIELD: &str = "v";

/// The name of the checkboxes that remove an item from a list of objects.
/// Each one's value is the form path of the item it removes.
const REMOVE_FIELD: &str = "remove";

/// Most items of a list of objects that one form may submit.
const MAX_FORM_ITEMS: usize = 1000;

/// Render the form fields for a value of `setting`, currently `value`.
/// `id` makes the fields' element IDs unique on the page.
///
/// Returns `None` if `value` cannot be shown in these fields, because it
/// does not match the setting; it should then be edited as JSON.
pub fn render_input(setting: &Setting, value: Option<&Value>, id: &str) -> Option<String> {
    let value = value.filter(|v| !v.is_null());
    if let Some(value) = value {
        if !fits(&setting.kind, value) {
            return None;
        }
    }
    let field = Field {
        path: VALUE_FIELD.to_owned(),
        id: format!("{id}-{VALUE_FIELD}"),
        optional: false,
    };
    Some(render_widget(&setting.kind, &field, value, setting.label()))
}

/// Where in a form a field is.
struct Field {
    /// The field's name, as described in the module docs.
    path: String,
    /// The field's element ID.
    id: String,
    /// Whether the value may be left unset, as an object's optional fields
    /// may.
    optional: bool,
}

impl Field {
    fn child(&self, part: &str, optional: bool) -> Self {
        Self {
            path: format!("{}.{part}", self.path),
            id: format!("{}.{part}", self.id),
            optional,
        }
    }
}

/// Whether `value` matches `kind` and can be shown in its form fields.
fn fits(kind: &SettingType, value: &Value) -> bool {
    if kind.check(value, &ValuePath::default()).is_err() {
        return false;
    }
    match (kind, value) {
        // One line per item: string items must be non-blank single lines.
        (SettingType::List { items }, Value::Array(list))
            if matches!(**items, SettingType::String { multiline: false }) =>
        {
            list.iter()
                .all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))
        }
        (SettingType::List { items }, Value::Array(list)) => match &**items {
            SettingType::Object { fields } => list
                .iter()
                .all(|item| item.as_object().is_some_and(|o| fits_object(fields, o))),
            _ => true,
        },
        (SettingType::Object { fields }, Value::Object(o)) => fits_object(fields, o),
        _ => true,
    }
}

fn fits_object(fields: &[Setting], object: &Map<String, Value>) -> bool {
    fields.iter().all(|f| {
        object
            .get(&f.name)
            .filter(|v| !v.is_null())
            .is_none_or(|v| fits(&f.kind, v) && !is_blank(&f.kind, v))
    })
}

/// Whether `value` would be read back from its form fields as unset, so an
/// optional field holding it would be dropped.
fn is_blank(kind: &SettingType, value: &Value) -> bool {
    match (kind, value) {
        (SettingType::String { .. }, Value::String(s)) => s.is_empty(),
        (SettingType::List { .. }, Value::Array(a)) => a.is_empty(),
        (SettingType::Object { fields }, Value::Object(o)) => fields.iter().all(|f| {
            o.get(&f.name)
                .is_none_or(|v| v.is_null() || is_blank(&f.kind, v))
        }),
        _ => false,
    }
}

/// Render the fields for a value of `kind` at `field`, labelled `label`.
fn render_widget(kind: &SettingType, field: &Field, value: Option<&Value>, label: &str) -> String {
    let Field { path, id, .. } = field;
    let aria = format!("aria-label=\"{}\"", escape(label));
    match kind {
        SettingType::String { multiline: true } => format!(
            "<textarea name=\"{path}\" id=\"{id}\" rows=\"4\" {aria}>{}</textarea>",
            escape(value.and_then(Value::as_str).unwrap_or_default())
        ),
        SettingType::String { multiline: false } | SettingType::Feed => format!(
            "<input type=\"text\" name=\"{path}\" id=\"{id}\" value=\"{}\" {aria}>",
            escape(value.map(line_text).unwrap_or_default())
        ),
        SettingType::Integer { min, max } => format!(
            "<input type=\"number\" step=\"1\"{}{} name=\"{path}\" id=\"{id}\" value=\"{}\" {aria}>",
            attr("min", min.map(|n| n.to_string())),
            attr("max", max.map(|n| n.to_string())),
            value.map(Value::to_string).unwrap_or_default()
        ),
        SettingType::Number { min, max } => format!(
            "<input type=\"number\" step=\"any\"{}{} name=\"{path}\" id=\"{id}\" value=\"{}\" {aria}>",
            attr("min", min.map(|n| n.to_string())),
            attr("max", max.map(|n| n.to_string())),
            value.map(Value::to_string).unwrap_or_default()
        ),
        SettingType::Boolean => format!(
            "<label class=\"check\"><input type=\"checkbox\" name=\"{path}\" id=\"{id}\" \
             value=\"true\"{}> {}</label>",
            checked(value == Some(&Value::Bool(true))),
            escape(label)
        ),
        SettingType::Choice { choices } => {
            let current = value.and_then(Value::as_str);
            let mut html = format!("<select name=\"{path}\" id=\"{id}\" {aria}>");
            if field.optional || current.is_none() {
                html.push_str("<option value=\"\">(not set)</option>");
            }
            for choice in choices {
                html.push_str(&format!(
                    "<option value=\"{0}\"{1}>{0}</option>",
                    escape(choice),
                    if current == Some(choice.as_str()) {
                        " selected"
                    } else {
                        ""
                    }
                ));
            }
            html.push_str("</select>");
            html
        }
        SettingType::List { items } => render_list(items, field, value, label),
        SettingType::Object { fields } => {
            let object = value.and_then(Value::as_object);
            let mut html = String::from("<div class=\"fields\">");
            for f in fields {
                html.push_str(&render_object_field(
                    f,
                    &field.child(&f.name, !f.required),
                    object.and_then(|o| o.get(&f.name)).filter(|v| !v.is_null()),
                ));
            }
            html.push_str("</div>");
            html
        }
        SettingType::Json => render_json(field, value, label),
    }
}

/// Render the fields for a list of `items` at `field`.
fn render_list(items: &SettingType, field: &Field, value: Option<&Value>, label: &str) -> String {
    let Field { path, id, .. } = field;
    let list = value.and_then(Value::as_array);
    match items {
        SettingType::Choice { choices } => {
            let mut html = format!("<fieldset class=\"choices\" id=\"{id}\">");
            html.push_str(&format!("<legend class=\"sr\">{}</legend>", escape(label)));
            for choice in choices {
                let on = list.is_some_and(|l| l.iter().any(|v| v.as_str() == Some(choice)));
                html.push_str(&format!(
                    "<label class=\"check\"><input type=\"checkbox\" name=\"{path}\" \
                     value=\"{0}\"{1}> {0}</label>",
                    escape(choice),
                    checked(on)
                ));
            }
            html.push_str("</fieldset>");
            html
        }
        SettingType::String { multiline: false }
        | SettingType::Integer { .. }
        | SettingType::Number { .. }
        | SettingType::Feed => {
            let lines: Vec<String> = list
                .map(|l| l.iter().map(line_text).collect())
                .unwrap_or_default();
            format!(
                "<textarea name=\"{path}\" id=\"{id}\" rows=\"{}\" aria-label=\"{}\">{}</textarea>\
                 <span class=\"hint\">One per line.</span>",
                (lines.len() + 1).clamp(3, 12),
                escape(label),
                escape(lines.join("\n"))
            )
        }
        SettingType::Object { fields } => {
            let list = list.map(Vec::as_slice).unwrap_or_default();
            let mut html = format!(
                "<div class=\"items\" id=\"{id}\">\
                 <input type=\"hidden\" name=\"{path}#count\" value=\"{}\">",
                list.len() + 1
            );
            for (i, item) in list.iter().enumerate() {
                let item_field = field.child(&i.to_string(), false);
                html.push_str(&format!(
                    "<fieldset class=\"item\"><legend>{} {}</legend>\
                     <input type=\"hidden\" name=\"{3}#present\" value=\"1\">{}\
                     <label class=\"check remove\"><input type=\"checkbox\" name=\"{REMOVE_FIELD}\" \
                     value=\"{3}\"> Remove</label></fieldset>",
                    escape(label),
                    i + 1,
                    render_widget(items, &item_field, Some(item), label),
                    item_field.path
                ));
            }
            let new_field = field.child(&list.len().to_string(), false);
            html.push_str(&format!(
                "<fieldset class=\"item new\"><legend>Add to {}</legend>{}\
                 <span class=\"hint\">Leave blank to add nothing.</span></fieldset></div>",
                escape(label),
                render_widget(
                    &SettingType::Object {
                        fields: fields.clone()
                    },
                    &new_field,
                    None,
                    label
                ),
            ));
            html
        }
        _ => render_json(field, value, label),
    }
}

/// Render `setting`, a field of an object, at `field`: its label, its
/// fields and its description.
fn render_object_field(setting: &Setting, field: &Field, value: Option<&Value>) -> String {
    let label = setting.label();
    let description = setting
        .description
        .as_deref()
        .map(|d| format!("<span class=\"hint\">{}</span>", escape(d)))
        .unwrap_or_default();
    let widget = render_widget(&setting.kind, field, value, label);
    match &setting.kind {
        // The checkbox carries its own label.
        SettingType::Boolean => format!("<div class=\"field\">{widget}{description}</div>"),
        kind if is_group(kind) => format!(
            "<fieldset class=\"field\"><legend>{}{}</legend>{widget}{description}</fieldset>",
            escape(label),
            required_mark(setting)
        ),
        _ => format!(
            "<div class=\"field\"><label for=\"{}\">{}{}</label>{widget}{description}</div>",
            field.id,
            escape(label),
            required_mark(setting)
        ),
    }
}

/// Render a textarea for `value` written as JSON, at `field`.
fn render_json(field: &Field, value: Option<&Value>, label: &str) -> String {
    format!(
        "<textarea name=\"{}\" id=\"{}\" rows=\"4\" class=\"json\" aria-label=\"{}\">{}</textarea>\
         <span class=\"hint\">As JSON.</span>",
        field.path,
        field.id,
        escape(label),
        escape(
            value
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()))
                .unwrap_or_default()
        )
    )
}

/// Whether the fields for `kind` are a group of several, labelled as a
/// whole by a legend rather than a label.
fn is_group(kind: &SettingType) -> bool {
    match kind {
        SettingType::Object { .. } => true,
        SettingType::List { items } => {
            matches!(
                **items,
                SettingType::Choice { .. } | SettingType::Object { .. }
            )
        }
        _ => false,
    }
}

fn required_mark(setting: &Setting) -> &'static str {
    if setting.required {
        " <abbr title=\"required\">*</abbr>"
    } else {
        ""
    }
}

/// `value` as it is written in a text field or on a line of a list: a
/// string as it is, anything else as JSON.
fn line_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

fn attr(name: &str, value: Option<String>) -> String {
    value
        .map(|v| format!(" {name}=\"{}\"", escape(&v)))
        .unwrap_or_default()
}

fn checked(on: bool) -> &'static str {
    if on {
        " checked"
    } else {
        ""
    }
}

/// The fields of a submitted form.
pub struct FormValues {
    values: HashMap<String, Vec<String>>,
    removed: HashSet<String>,
}

impl FormValues {
    /// Collects the fields of a form from its name and value pairs.
    pub fn new(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut values: HashMap<String, Vec<String>> = HashMap::new();
        let mut removed = HashSet::new();
        for (name, value) in pairs {
            if name == REMOVE_FIELD {
                removed.insert(value);
            } else {
                values.entry(name).or_default().push(value);
            }
        }
        Self { values, removed }
    }

    /// The first value of field `name`, if it was submitted.
    pub fn first(&self, name: &str) -> Option<&str> {
        self.values.get(name)?.first().map(String::as_str)
    }

    fn all(&self, name: &str) -> &[String] {
        self.values.get(name).map(Vec::as_slice).unwrap_or_default()
    }
}

/// Reads the value of `setting`, config key `key`, from the fields that
/// [`render_input`] rendered for it, as submitted in `form`.
///
/// # Errors
///
/// Returns an [`InvalidValue`] if a field holds something that is not of
/// its type, a required field is empty, or the value does not match the
/// setting.
pub fn parse_input(setting: &Setting, key: &str, form: &FormValues) -> Result<Value, InvalidValue> {
    let path = ValuePath::key(key);
    let value = match parse(&setting.kind, VALUE_FIELD, &path, form)? {
        Some(value) => value,
        None => match &setting.kind {
            SettingType::String { .. } => Value::String(String::new()),
            kind => empty(kind, &path)?.ok_or_else(|| InvalidValue {
                path: path.clone(),
                message: "enter a value, or reset the setting to its default".into(),
            })?,
        },
    };
    setting.kind.check(&value, &path)?;
    Ok(value)
}

/// Reads a value of `kind` from the fields at form path `name`, for the
/// config value at `path`. Returns `None` if the fields were left empty.
fn parse(
    kind: &SettingType,
    name: &str,
    path: &ValuePath,
    form: &FormValues,
) -> Result<Option<Value>, InvalidValue> {
    let bad = |message: String| InvalidValue {
        path: path.clone(),
        message,
    };
    let text = form.first(name).unwrap_or_default();
    Ok(match kind {
        SettingType::String { .. } => Some(text.replace("\r\n", "\n"))
            .filter(|s| !s.is_empty())
            .map(Value::String),
        SettingType::Integer { .. } => parse_integer(text.trim()).map_err(bad)?,
        SettingType::Number { .. } => parse_number(text.trim()).map_err(bad)?,
        SettingType::Boolean => form
            .all(name)
            .iter()
            .any(|v| v == "true")
            .then_some(Value::Bool(true)),
        SettingType::Choice { .. } => Some(text).filter(|s| !s.is_empty()).map(Value::from),
        SettingType::Feed => parse_feed(text.trim()),
        SettingType::List { items } => parse_list(items, name, path, form)?,
        SettingType::Object { fields } => parse_object(fields, name, path, form, false)?,
        SettingType::Json => parse_json(text).map_err(bad)?,
    })
}

fn parse_integer(text: &str) -> Result<Option<Value>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<i64>()
        .map(|n| Some(Value::from(n)))
        .map_err(|_| format!("{text:?} is not a whole number"))
}

fn parse_number(text: &str) -> Result<Option<Value>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    if let Ok(n) = text.parse::<i64>() {
        return Ok(Some(Value::from(n)));
    }
    text.parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .map(|n| Some(Value::Number(n)))
        .ok_or_else(|| format!("{text:?} is not a number"))
}

/// Reads a feed: an id if `text` is a whole number, and otherwise a URL.
fn parse_feed(text: &str) -> Option<Value> {
    if text.is_empty() {
        return None;
    }
    Some(match text.parse::<i64>() {
        Ok(id) => Value::from(id),
        Err(_) => Value::from(text),
    })
}

fn parse_json(text: &str) -> Result<Option<Value>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(text)
        .map(Some)
        .map_err(|e| format!("not valid JSON ({e})"))
}

fn parse_list(
    items: &SettingType,
    name: &str,
    path: &ValuePath,
    form: &FormValues,
) -> Result<Option<Value>, InvalidValue> {
    let mut list = Vec::new();
    match items {
        SettingType::Choice { .. } => {
            list.extend(
                form.all(name)
                    .iter()
                    .filter(|v| !v.is_empty())
                    .map(|v| Value::from(v.as_str())),
            );
        }
        SettingType::String { multiline: false }
        | SettingType::Integer { .. }
        | SettingType::Number { .. }
        | SettingType::Feed => {
            let text = form.first(name).unwrap_or_default();
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let item_path = path.index(list.len());
                let bad = |message| InvalidValue {
                    path: item_path.clone(),
                    message,
                };
                list.extend(match items {
                    SettingType::Integer { .. } => parse_integer(line.trim()).map_err(bad)?,
                    SettingType::Number { .. } => parse_number(line.trim()).map_err(bad)?,
                    SettingType::Feed => parse_feed(line.trim()),
                    _ => Some(Value::from(line.trim_end_matches('\r'))),
                });
            }
        }
        SettingType::Object { fields } => {
            let count = form
                .first(&format!("{name}#count"))
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0)
                .min(MAX_FORM_ITEMS);
            for i in 0..count {
                let item_name = format!("{name}.{i}");
                if form.removed.contains(&item_name) {
                    continue;
                }
                let item_path = path.index(list.len());
                // Items that were already in the list are kept even if all
                // their fields are left empty or unticked; only the blank
                // fieldset for a new item is dropped when left blank.
                let existing = form.first(&format!("{item_name}#present")).is_some();
                list.extend(parse_object(
                    fields, &item_name, &item_path, form, existing,
                )?);
            }
        }
        _ => {
            return parse_json(form.first(name).unwrap_or_default()).map_err(|message| {
                InvalidValue {
                    path: path.clone(),
                    message,
                }
            })
        }
    }
    Ok((!list.is_empty()).then_some(Value::Array(list)))
}

/// Reads an object with `fields` from the fields at form path `name`.
/// Returns `None` if every field was left empty, unless `keep` is set.
fn parse_object(
    fields: &[Setting],
    name: &str,
    path: &ValuePath,
    form: &FormValues,
    keep: bool,
) -> Result<Option<Value>, InvalidValue> {
    let mut parsed = Vec::with_capacity(fields.len());
    for field in fields {
        let value = parse(
            &field.kind,
            &format!("{name}.{}", field.name),
            &path.field(&field.name),
            form,
        )?;
        parsed.push(value);
    }
    if !keep && parsed.iter().all(Option::is_none) {
        return Ok(None);
    }
    fill_object(fields, parsed, path).map(Some)
}

/// Builds an object from the values read for its `fields`, giving each
/// field left empty its empty value, and failing if a required one has
/// none.
fn fill_object(
    fields: &[Setting],
    parsed: Vec<Option<Value>>,
    path: &ValuePath,
) -> Result<Value, InvalidValue> {
    let mut object = Map::new();
    for (field, value) in fields.iter().zip(parsed) {
        let field_path = path.field(&field.name);
        let value =
            match value {
                Some(value) => Some(value),
                // An unticked box is false, whether the field is required or not.
                None if matches!(field.kind, SettingType::Boolean) => Some(Value::Bool(false)),
                None if field.required => Some(empty(&field.kind, &field_path)?.ok_or_else(
                    || InvalidValue {
                        path: field_path.clone(),
                        message: "is required".into(),
                    },
                )?),
                None => None,
            };
        if let Some(value) = value {
            object.insert(field.name.clone(), value);
        }
    }
    Ok(Value::Object(object))
}

/// The value that empty fields for `kind` stand for, when one must be
/// given: `false`, an empty list, or an object of empty fields.
fn empty(kind: &SettingType, path: &ValuePath) -> Result<Option<Value>, InvalidValue> {
    Ok(match kind {
        SettingType::Boolean => Some(Value::Bool(false)),
        SettingType::List { .. } => Some(Value::Array(vec![])),
        SettingType::Object { fields } => {
            Some(fill_object(fields, vec![None; fields.len()], path)?)
        }
        _ => None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn feeds() -> Setting {
        Setting {
            name: "feeds".into(),
            label: None,
            description: None,
            required: false,
            kind: SettingType::List {
                items: Box::new(SettingType::Feed),
            },
        }
    }

    /// A list of feeds is one per line: whole numbers are feed ids, and
    /// anything else a URL.
    #[test]
    fn feeds_are_read_one_per_line() {
        let form = FormValues::new([(
            "v".to_owned(),
            "3\r\n https://example.com/feed.xml \r\n\r\n".to_owned(),
        )]);
        assert_eq!(
            parse_input(&feeds(), "feeds", &form).unwrap(),
            json!([3, "https://example.com/feed.xml"])
        );

        let form = FormValues::new([("v".to_owned(), "0".to_owned())]);
        assert_eq!(
            parse_input(&feeds(), "feeds", &form)
                .unwrap_err()
                .to_string(),
            "feeds[0]: expected a feed id or URL"
        );
    }

    #[test]
    fn feeds_are_shown_one_per_line() {
        let value = json!([3, "https://example.com/feed.xml"]);
        let html = render_input(&feeds(), Some(&value), "s").unwrap();
        assert!(
            html.contains(">3\nhttps://example.com/feed.xml</textarea>"),
            "{html}"
        );
    }
}
