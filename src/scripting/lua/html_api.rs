//! The `kiki.html` API: rewriting HTML from scripts, backed by `lol_html`.
//!
//! Entry content is HTML, and Lua's string patterns cannot parse it: a pattern sees
//! `<a href="x">` where a browser sees something else, which is how sanitizers built on
//! patterns get bypassed. `kiki.html.rewrite(html, handlers)` parses `html` with the same
//! streaming rewriter Kiki uses for its own HTML, calls the script's handlers for the
//! elements matching their CSS selectors, and for comments and text, and returns the
//! rewritten HTML.
//!
//! # Handler objects
//!
//! A handler is passed an object describing its element, comment or text chunk. The
//! object holds a copy of what the handler can read, and records the changes the handler
//! makes, which are applied to the rewriter's own element once the handler returns.
//! (Lending the rewriter's element to Lua directly would need a userdata type with a
//! borrowed lifetime, and mlua builds a metatable afresh for every value of such a type,
//! which costs far more than the rest of the rewrite.) The object is spent once its
//! handler returns: using it afterwards raises an error.
//!
//! # Attribute values
//!
//! `lol_html` hands over attribute values as they appear in the source, character
//! references and all, and writes values back escaping only double quotes. Scripts instead
//! see decoded values: `get_attribute` decodes character references, as a browser would,
//! and `set_attribute` escapes `&` and `"`, so that the value a script sets is the value a
//! browser reads back. A script that checks a value and then sets it again therefore
//! writes exactly what it checked, even where [`decode_attribute`] decodes a value
//! differently from a browser.
//!
//! # Resource limits
//!
//! The rewriter's buffers and output live outside the Lua allocator, so the VM's memory
//! cap does not see them. They are bounded here instead: the rewriter may use at most
//! [`HTML_MEMORY_LIMIT_BYTES`] for its buffers, the rewritten HTML may be at most
//! [`HTML_OUTPUT_LIMIT_BYTES`] long, and a rewrite may have at most [`MAX_SELECTORS`]
//! element handlers. Handlers run under the time budget of the handler that called
//! `kiki.html.rewrite`.

use lol_html::errors::RewritingError;
use lol_html::html_content::{Comment, ContentType, Element, TextChunk};
use lol_html::{
    DocumentContentHandlers, ElementContentHandlers, HtmlRewriter, MemorySettings, Selector,
    Settings,
};
use mlua::prelude::*;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};

/// Most memory the rewriter may use for its buffers during one rewrite.
pub const HTML_MEMORY_LIMIT_BYTES: usize = 4 * 1024 * 1024;

/// Longest HTML a rewrite may produce.
pub const HTML_OUTPUT_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// Most element handlers one rewrite may have.
pub const MAX_SELECTORS: usize = 256;

/// Install `kiki.html` into the `kiki` table.
pub(super) fn install(lua: &Lua, kiki: &LuaTable) -> LuaResult<()> {
    let module = lua.create_table()?;
    module.set(
        "rewrite",
        lua.create_function(|lua, (html, handlers): (LuaString, LuaTable)| {
            rewrite(lua, &html.as_bytes(), &handlers)
        })?,
    )?;
    module.set(
        "escape",
        lua.create_function(|_, s: LuaString| Ok(escape(&s.to_str()?)))?,
    )?;
    module.set(
        "unescape",
        lua.create_function(|_, s: LuaString| Ok(decode_attribute(&s.to_str()?).into_owned()))?,
    )?;
    kiki.set("html", module)
}

/// The error a content handler returns to stop the rewriter. What went wrong is kept
/// aside, in [`Rewrite::error`], so that the Lua error, such as a timeout, reaches the
/// script unchanged.
#[derive(Debug, thiserror::Error)]
#[error("rewrite aborted")]
struct Aborted;

/// The state a rewrite's handlers share.
#[derive(Default)]
struct Rewrite {
    /// The first error a Lua handler raised.
    error: RefCell<Option<LuaError>>,
    /// Whether the output grew past [`HTML_OUTPUT_LIMIT_BYTES`].
    overflowed: Cell<bool>,
}

impl Rewrite {
    /// Call `handler` with `node`, then apply the edits it recorded with `apply`. An error
    /// stops the rewriter.
    fn run<T: Node>(
        &self,
        lua: &Lua,
        handler: &LuaFunction,
        node: T,
        apply: impl FnOnce(Vec<Edit>) -> LuaResult<()>,
    ) -> lol_html::HandlerResult {
        if self.overflowed.get() {
            return Err(Box::new(Aborted));
        }
        let result = lua.create_userdata(node).and_then(|ud| {
            handler.call::<()>(&ud)?;
            // Taking the value out spends the object, so a script that kept it gets an
            // error rather than a stale copy.
            apply(ud.take::<T>()?.into_edits())
        });
        result.map_err(|e| {
            self.error.borrow_mut().get_or_insert(e);
            Box::new(Aborted) as _
        })
    }
}

/// `kiki.html.rewrite(html, handlers)`.
fn rewrite(lua: &Lua, html: &[u8], handlers: &LuaTable) -> LuaResult<LuaString> {
    let state = Rewrite::default();

    let mut element_handlers = Vec::new();
    if let Some(elements) = handlers.get::<Option<LuaTable>>("elements")? {
        for (i, pair) in elements.sequence_values::<LuaTable>().enumerate() {
            let pair = pair.map_err(|_| {
                runtime_error(format!(
                    "elements[{}] must be a {{selector, handler}} pair",
                    i + 1
                ))
            })?;
            let selector: String = pair.get(1).map_err(|_| {
                runtime_error(format!("elements[{}][1] must be a CSS selector", i + 1))
            })?;
            let handler: LuaFunction = pair
                .get(2)
                .map_err(|_| runtime_error(format!("elements[{}][2] must be a function", i + 1)))?;
            if element_handlers.len() == MAX_SELECTORS {
                return Err(runtime_error(format!(
                    "too many element handlers (at most {MAX_SELECTORS})"
                )));
            }
            let selector: Selector = selector
                .parse()
                .map_err(|e| runtime_error(format!("invalid selector {selector:?}: {e}")))?;
            let state = &state;
            element_handlers.push((
                Cow::Owned(selector),
                ElementContentHandlers::default().element(move |el: &mut Element| {
                    state.run(lua, &handler, LuaElement::new(el), |edits| {
                        apply_to_element(el, edits)
                    })
                }),
            ));
        }
    }

    let mut document = DocumentContentHandlers::default();
    if let Some(handler) = handlers.get::<Option<LuaFunction>>("comments")? {
        let state = &state;
        document = document.comments(move |c: &mut Comment| {
            state.run(lua, &handler, LuaComment::new(c), |edits| {
                apply_to_comment(c, edits)
            })
        });
    }
    if let Some(handler) = handlers.get::<Option<LuaFunction>>("text")? {
        let state = &state;
        document = document.text(move |t: &mut TextChunk| {
            state.run(lua, &handler, LuaText::new(t), |edits| {
                apply_to_text(t, edits);
                Ok(())
            })
        });
    }

    let mut out = Vec::new();
    let overflowed = &state.overflowed;
    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: element_handlers,
            document_content_handlers: vec![document],
            memory_settings: MemorySettings {
                max_allowed_memory_usage: HTML_MEMORY_LIMIT_BYTES,
                ..MemorySettings::default()
            },
            ..Settings::new()
        },
        |chunk: &[u8]| {
            if overflowed.get() || out.len() + chunk.len() > HTML_OUTPUT_LIMIT_BYTES {
                overflowed.set(true);
            } else {
                out.extend_from_slice(chunk);
            }
        },
    );
    let result = rewriter.write(html).and_then(|()| rewriter.end());

    if let Some(e) = state.error.take() {
        return Err(e);
    }
    if state.overflowed.get() {
        return Err(runtime_error(format!(
            "the rewritten HTML is longer than {HTML_OUTPUT_LIMIT_BYTES} bytes"
        )));
    }
    result.map_err(|e| match e {
        RewritingError::MemoryLimitExceeded(_) => {
            runtime_error("the HTML needs too much memory to rewrite")
        }
        other => runtime_error(format!("unable to rewrite the HTML: {other}")),
    })?;
    lua.create_string(out)
}

fn runtime_error(message: impl std::fmt::Display) -> LuaError {
    LuaError::RuntimeError(format!("kiki.html: {message}"))
}

/// A change a handler made to its element, comment or text chunk, to be applied once the
/// handler returns.
enum Edit {
    /// Insert content at a place, as HTML if the flag is set or else as text.
    Insert(Place, String, bool),
    /// Remove the node, with its content.
    Remove,
    /// Remove an element's tags, keeping its content.
    Unwrap,
    /// Set an attribute to a value, or remove it.
    Attribute(String, Option<String>),
    /// Rename an element.
    TagName(String),
    /// Set a comment's text.
    Text(String),
}

/// Where [`Edit::Insert`] puts its content.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Before,
    After,
    Prepend,
    Append,
    /// In place of an element's content.
    Inner,
    /// In place of the node.
    Replace,
}

impl Place {
    /// The places every kind of node takes, by the name of the method that inserts there.
    const ANY_NODE: &[(&'static str, Place)] = &[
        ("before", Place::Before),
        ("after", Place::After),
        ("replace", Place::Replace),
    ];

    /// The places only elements take.
    const ELEMENT: &[(&'static str, Place)] = &[
        ("prepend", Place::Prepend),
        ("append", Place::Append),
        ("set_inner_content", Place::Inner),
    ];
}

fn content_type(html: bool) -> ContentType {
    if html {
        ContentType::Html
    } else {
        ContentType::Text
    }
}

/// The Lua object for an element, comment or text chunk.
trait Node: LuaUserData + Send + Sized + 'static {
    /// The edits recorded so far.
    fn edits(&mut self) -> &mut Vec<Edit>;
    /// Note that the node was removed or replaced.
    fn set_removed(&mut self);
    fn into_edits(self) -> Vec<Edit>;
}

/// Add the methods every kind of node has: inserting content at `places` and removing the
/// node.
fn add_node_methods<T: Node, M: LuaUserDataMethods<T>>(
    methods: &mut M,
    places: &'static [(&'static str, Place)],
) {
    for &(name, place) in places {
        methods.add_method_mut(
            name,
            move |_, this, (content, kind): (String, Option<String>)| {
                let html = match kind.as_deref() {
                    None | Some("text") => false,
                    Some("html") => true,
                    Some(other) => {
                        return Err(runtime_error(format!(
                            "unknown content type {other:?}; expected \"text\" or \"html\""
                        )))
                    }
                };
                if place == Place::Replace {
                    this.set_removed();
                }
                this.edits().push(Edit::Insert(place, content, html));
                Ok(())
            },
        );
    }
    methods.add_method_mut("remove", |_, this, ()| {
        this.set_removed();
        this.edits().push(Edit::Remove);
        Ok(())
    });
}

/// Check `name` as `lol_html` would before using it as an attribute name, so that a bad
/// name raises its error where the script gave it.
fn check_attribute_name(name: &str) -> LuaResult<()> {
    if name.is_empty() || name.contains([' ', '\n', '\r', '\t', '\x0C', '/', '>', '=']) {
        return Err(runtime_error(format!("invalid attribute name {name:?}")));
    }
    Ok(())
}

/// Check `name` as `lol_html` would before using it as a tag name.
fn check_tag_name(name: &str) -> LuaResult<()> {
    if !name.starts_with(|c: char| c.is_ascii_alphabetic())
        || name.contains([' ', '\n', '\r', '\t', '\x0C', '/', '>'])
    {
        return Err(runtime_error(format!("invalid tag name {name:?}")));
    }
    Ok(())
}

/// The Lua object for an element.
struct LuaElement {
    tag_name: String,
    namespace: &'static str,
    is_self_closing: bool,
    can_have_content: bool,
    /// The element's attributes, with lowercase names and decoded values, as the
    /// handler's edits have left them.
    attributes: Vec<(String, String)>,
    removed: bool,
    edits: Vec<Edit>,
}

impl LuaElement {
    fn new(el: &Element) -> Self {
        Self {
            tag_name: el.tag_name(),
            namespace: match el.namespace_uri() {
                "http://www.w3.org/2000/svg" => "svg",
                "http://www.w3.org/1998/Math/MathML" => "mathml",
                _ => "html",
            },
            is_self_closing: el.is_self_closing(),
            can_have_content: el.can_have_content(),
            attributes: el
                .attributes()
                .iter()
                .map(|a| (a.name(), decode_attribute(&a.value()).into_owned()))
                .collect(),
            removed: el.removed(),
            edits: Vec::new(),
        }
    }

    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl Node for LuaElement {
    fn edits(&mut self) -> &mut Vec<Edit> {
        &mut self.edits
    }

    fn set_removed(&mut self) {
        self.removed = true;
    }

    fn into_edits(self) -> Vec<Edit> {
        self.edits
    }
}

impl LuaUserData for LuaElement {
    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("tag_name", |_, this| Ok(this.tag_name.clone()));
        fields.add_field_method_get("namespace", |_, this| Ok(this.namespace));
        fields.add_field_method_get("is_self_closing", |_, this| Ok(this.is_self_closing));
        fields.add_field_method_get("can_have_content", |_, this| Ok(this.can_have_content));
        fields.add_field_method_get("removed", |_, this| Ok(this.removed));
    }

    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get_attribute", |_, this, name: String| {
            Ok(this.attribute(&name).map(str::to_owned))
        });
        methods.add_method("has_attribute", |_, this, name: String| {
            Ok(this.attribute(&name).is_some())
        });
        methods.add_method("attributes", |lua, this, ()| {
            lua.create_sequence_from(
                this.attributes
                    .iter()
                    .map(|(name, value)| {
                        let t = lua.create_table()?;
                        t.set("name", name.as_str())?;
                        t.set("value", value.as_str())?;
                        Ok(t)
                    })
                    .collect::<LuaResult<Vec<_>>>()?,
            )
        });
        methods.add_method_mut(
            "set_attribute",
            |_, this, (name, value): (String, String)| {
                check_attribute_name(&name)?;
                let lower = name.to_ascii_lowercase();
                match this.attributes.iter_mut().find(|(n, _)| *n == lower) {
                    Some((_, v)) => v.clone_from(&value),
                    None => this.attributes.push((lower, value.clone())),
                }
                this.edits.push(Edit::Attribute(name, Some(value)));
                Ok(())
            },
        );
        methods.add_method_mut("remove_attribute", |_, this, name: String| {
            this.attributes
                .retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
            this.edits.push(Edit::Attribute(name, None));
            Ok(())
        });
        methods.add_method_mut("set_tag_name", |_, this, name: String| {
            check_tag_name(&name)?;
            this.tag_name = name.to_ascii_lowercase();
            this.edits.push(Edit::TagName(name));
            Ok(())
        });
        methods.add_method_mut("remove_and_keep_content", |_, this, ()| {
            this.removed = true;
            this.edits.push(Edit::Unwrap);
            Ok(())
        });
        add_node_methods(methods, Place::ANY_NODE);
        add_node_methods(methods, Place::ELEMENT);

        methods.add_meta_method(LuaMetaMethod::ToString, |_, this, ()| {
            Ok(format!("kiki.html element <{}>", this.tag_name))
        });
    }
}

fn apply_to_element(el: &mut Element, edits: Vec<Edit>) -> LuaResult<()> {
    for edit in edits {
        match edit {
            Edit::Insert(place, content, html) => {
                let ct = content_type(html);
                match place {
                    Place::Before => el.before(&content, ct),
                    Place::After => el.after(&content, ct),
                    Place::Prepend => el.prepend(&content, ct),
                    Place::Append => el.append(&content, ct),
                    Place::Inner => el.set_inner_content(&content, ct),
                    Place::Replace => el.replace(&content, ct),
                }
            }
            Edit::Remove => el.remove(),
            Edit::Unwrap => el.remove_and_keep_content(),
            Edit::Attribute(name, Some(value)) => el
                .set_attribute(&name, &escape_attribute(&value))
                .map_err(|e| runtime_error(format!("invalid attribute name {name:?}: {e}")))?,
            Edit::Attribute(name, None) => el.remove_attribute(&name),
            Edit::TagName(name) => el
                .set_tag_name(&name)
                .map_err(|e| runtime_error(format!("invalid tag name {name:?}: {e}")))?,
            // Elements have no text of their own.
            Edit::Text(_) => {}
        }
    }
    Ok(())
}

/// The Lua object for a comment.
struct LuaComment {
    text: String,
    removed: bool,
    edits: Vec<Edit>,
}

impl LuaComment {
    fn new(c: &Comment) -> Self {
        Self {
            text: c.text(),
            removed: c.removed(),
            edits: Vec::new(),
        }
    }
}

impl Node for LuaComment {
    fn edits(&mut self) -> &mut Vec<Edit> {
        &mut self.edits
    }

    fn set_removed(&mut self) {
        self.removed = true;
    }

    fn into_edits(self) -> Vec<Edit> {
        self.edits
    }
}

impl LuaUserData for LuaComment {
    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("text", |_, this| Ok(this.text.clone()));
        fields.add_field_method_get("removed", |_, this| Ok(this.removed));
    }

    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("set_text", |_, this, text: String| {
            if text.contains("-->") {
                return Err(runtime_error(
                    "invalid comment text: it may not contain \"-->\"",
                ));
            }
            this.text.clone_from(&text);
            this.edits.push(Edit::Text(text));
            Ok(())
        });
        add_node_methods(methods, Place::ANY_NODE);
    }
}

fn apply_to_comment(c: &mut Comment, edits: Vec<Edit>) -> LuaResult<()> {
    for edit in edits {
        match edit {
            Edit::Insert(place, content, html) => {
                let ct = content_type(html);
                match place {
                    Place::Before => c.before(&content, ct),
                    Place::After => c.after(&content, ct),
                    _ => c.replace(&content, ct),
                }
            }
            Edit::Remove => c.remove(),
            Edit::Text(text) => c
                .set_text(&text)
                .map_err(|e| runtime_error(format!("invalid comment text: {e}")))?,
            // Comments have no tags or attributes.
            Edit::Unwrap | Edit::Attribute(..) | Edit::TagName(_) => {}
        }
    }
    Ok(())
}

/// The Lua object for a chunk of text.
struct LuaText {
    text: String,
    last_in_text_node: bool,
    removed: bool,
    edits: Vec<Edit>,
}

impl LuaText {
    fn new(t: &TextChunk) -> Self {
        Self {
            text: t.as_str().to_owned(),
            last_in_text_node: t.last_in_text_node(),
            removed: t.removed(),
            edits: Vec::new(),
        }
    }
}

impl Node for LuaText {
    fn edits(&mut self) -> &mut Vec<Edit> {
        &mut self.edits
    }

    fn set_removed(&mut self) {
        self.removed = true;
    }

    fn into_edits(self) -> Vec<Edit> {
        self.edits
    }
}

impl LuaUserData for LuaText {
    fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("text", |_, this| Ok(this.text.clone()));
        fields.add_field_method_get("last_in_text_node", |_, this| Ok(this.last_in_text_node));
        fields.add_field_method_get("removed", |_, this| Ok(this.removed));
    }

    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        add_node_methods(methods, Place::ANY_NODE);
    }
}

fn apply_to_text(t: &mut TextChunk, edits: Vec<Edit>) {
    for edit in edits {
        match edit {
            Edit::Insert(place, content, html) => {
                let ct = content_type(html);
                match place {
                    Place::Before => t.before(&content, ct),
                    Place::After => t.after(&content, ct),
                    _ => t.replace(&content, ct),
                }
            }
            Edit::Remove => t.remove(),
            // Text has no tags, attributes or text of its own to set.
            Edit::Unwrap | Edit::Attribute(..) | Edit::TagName(_) | Edit::Text(_) => {}
        }
    }
}

/// Escape `s` for use as text or as a quoted attribute value in HTML.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Escape `s` for use as a double-quoted attribute value. `lol_html` escapes the double
/// quotes itself, so only `&` is escaped here: escaping `"` too would escape it twice.
fn escape_attribute(s: &str) -> Cow<'_, str> {
    if s.contains('&') {
        Cow::Owned(s.replace('&', "&amp;"))
    } else {
        Cow::Borrowed(s)
    }
}

/// Decode the character references in `s`, an attribute value, the way a browser does.
///
/// Numeric references are decoded with or without their terminating `;`, with the code
/// points the HTML standard replaces mapped as it says. Named references are decoded when
/// they end in `;`; the few legacy names a browser also decodes without the `;` are left
/// as they are.
fn decode_attribute(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some((before, after)) = rest.split_once('&') {
        out.push_str(before);
        match decode_reference(after) {
            Some((decoded, len)) => {
                out.push_str(&decoded);
                rest = after.get(len..).unwrap_or_default();
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Decode the character reference at the start of `s`, the text following an `&`,
/// returning what it stands for and how much of `s` it takes up, or `None` if `s` does not
/// start with one.
fn decode_reference(s: &str) -> Option<(Cow<'static, str>, usize)> {
    if let Some(number) = s.strip_prefix('#') {
        let (radix, digits, prefix_len) = match number.strip_prefix(['x', 'X']) {
            Some(hex) => (16, hex, 2),
            None => (10, number, 1),
        };
        let digits_len = digits.chars().take_while(|c| c.is_digit(radix)).count();
        if digits_len == 0 {
            return None;
        }
        // Too many digits for a u32 is past the last code point all the same.
        let code = u32::from_str_radix(digits.get(..digits_len)?, radix).unwrap_or(u32::MAX);
        let semicolon = digits.get(digits_len..)?.starts_with(';');
        let len = prefix_len + digits_len + usize::from(semicolon);
        return Some((Cow::Owned(numeric_reference(code).to_string()), len));
    }

    let name_len = s.bytes().take_while(u8::is_ascii_alphanumeric).count();
    let name = s.get(..name_len)?;
    if name.is_empty() || !s.get(name_len..)?.starts_with(';') {
        return None;
    }
    let decoded = quick_xml::escape::resolve_html5_entity(name)?;
    Some((Cow::Borrowed(decoded), name_len + 1))
}

/// The character the numeric character reference for `code` stands for, as the HTML
/// standard has it.
fn numeric_reference(code: u32) -> char {
    /// What the references to 0x80 to 0x9F stand for: the characters Windows-1252 has
    /// there, or `None` to keep the code point.
    const WINDOWS_1252: [Option<char>; 32] = [
        Some('\u{20AC}'),
        None,
        Some('\u{201A}'),
        Some('\u{0192}'),
        Some('\u{201E}'),
        Some('\u{2026}'),
        Some('\u{2020}'),
        Some('\u{2021}'),
        Some('\u{02C6}'),
        Some('\u{2030}'),
        Some('\u{0160}'),
        Some('\u{2039}'),
        Some('\u{0152}'),
        None,
        Some('\u{017D}'),
        None,
        None,
        Some('\u{2018}'),
        Some('\u{2019}'),
        Some('\u{201C}'),
        Some('\u{201D}'),
        Some('\u{2022}'),
        Some('\u{2013}'),
        Some('\u{2014}'),
        Some('\u{02DC}'),
        Some('\u{2122}'),
        Some('\u{0161}'),
        Some('\u{203A}'),
        Some('\u{0153}'),
        None,
        Some('\u{017E}'),
        Some('\u{0178}'),
    ];
    if code == 0 {
        return char::REPLACEMENT_CHARACTER;
    }
    if let Some(index) = code.checked_sub(0x80).filter(|i| *i < 32) {
        if let Some(Some(c)) = WINDOWS_1252.get(index as usize) {
            return *c;
        }
    }
    char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER)
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

    /// Rewrite `html` with the handlers table written in Lua as `handlers`.
    fn rewrite_with(html: &str, handlers: &str) -> LuaResult<String> {
        let lua = lua();
        lua.globals().set("html", html).unwrap();
        eval(&lua, &format!("return kiki.html.rewrite(html, {handlers})"))
    }

    #[test]
    fn without_handlers_html_is_unchanged() {
        let html = "<p class=x>Some &amp; <b>text</b><!-- c --></p>";
        assert_eq!(rewrite_with(html, "{}").unwrap(), html);
    }

    #[test]
    fn element_handlers_match_selectors_in_order() {
        let out = rewrite_with(
            r#"<p>a <script>x()</script><a href="/x" onclick="y()">b</a></p>"#,
            r#"{ elements = {
                { "script", function(el) el:remove() end },
                { "a[href]", function(el) el:remove_attribute("onclick") end },
                { "a", function(el) el:set_attribute("rel", "nofollow") end },
            } }"#,
        )
        .unwrap();
        assert_eq!(out, r#"<p>a <a href="/x" rel="nofollow">b</a></p>"#);
    }

    #[test]
    fn elements_expose_their_name_and_attributes() {
        let lua = lua();
        let (name, href, missing, has, count, first): (String, String, Option<String>, bool, i64, String) =
            eval(
                &lua,
                r#"
                local r = {}
                kiki.html.rewrite([[<A HREF="/a?x=1&amp;y=2" Title="t">x</A>]], { elements = {
                    { "*", function(el)
                        local attrs = el:attributes()
                        r = { el.tag_name, el:get_attribute("href"), el:get_attribute("nope"),
                              el:has_attribute("title"), #attrs, attrs[1].name .. "=" .. attrs[1].value }
                    end },
                } })
                return table.unpack(r, 1, 6)
                "#,
            )
            .unwrap();
        assert_eq!(name, "a");
        assert_eq!(href, "/a?x=1&y=2");
        assert_eq!(missing, None);
        assert!(has);
        assert_eq!(count, 2);
        assert_eq!(first, "href=/a?x=1&y=2");
    }

    #[test]
    fn attribute_values_round_trip() {
        // A value a script reads and sets again reads back the same.
        let out = rewrite_with(
            r#"<a href="/a?x=1&amp;y=&quot;2&quot;" title='it&#39;s'>x</a>"#,
            r#"{ elements = { { "a", function(el)
                for _, a in ipairs(el:attributes()) do
                    el:remove_attribute(a.name)
                    el:set_attribute(a.name, a.value)
                end
            end } } }"#,
        )
        .unwrap();
        assert_eq!(
            out,
            r#"<a href="/a?x=1&amp;y=&quot;2&quot;" title="it's">x</a>"#
        );
    }

    #[test]
    fn character_references_in_attributes_are_decoded_like_a_browser() {
        let cases = [
            ("&#106;avascript:", "javascript:"),
            ("&#106avascript:", "javascript:"),
            ("&#x6A;&#X61;vascript&colon;", "javascript:"),
            ("java&Tab;script:", "java\tscript:"),
            ("a&amp;b&lt;", "a&b<"),
            (
                "&#128;&#0;&#xD800;&#99999999999;",
                "\u{20AC}\u{FFFD}\u{FFFD}\u{FFFD}",
            ),
            ("&nope; & &# &#x; &amp", "&nope; & &# &#x; &amp"),
            ("plain", "plain"),
        ];
        for (raw, decoded) in cases {
            assert_eq!(decode_attribute(raw), decoded, "{raw:?}");
        }
    }

    #[test]
    fn content_can_be_inserted_as_text_or_html() {
        let out = rewrite_with(
            "<p>x</p>",
            r#"{ elements = { { "p", function(el)
                el:before("<b>", "html")
                el:after("</b>")
                el:prepend("[")
                el:append("]", "text")
            end } } }"#,
        )
        .unwrap();
        assert_eq!(out, "<b><p>[x]</p>&lt;/b&gt;");

        let out = rewrite_with(
            "<div><p>x</p></div><span>y</span>",
            r#"{ elements = {
                { "div", function(el) el:set_inner_content("<i>z</i>", "html") end },
                { "span", function(el) el:replace("a & b") end },
            } }"#,
        )
        .unwrap();
        assert_eq!(out, "<div><i>z</i></div>a &amp; b");

        let err = rewrite_with(
            "<p>x</p>",
            r#"{ elements = { { "p", function(el) el:append("x", "xml") end } } }"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown content type"), "{err}");
    }

    #[test]
    fn elements_can_be_unwrapped_and_renamed() {
        let out = rewrite_with(
            r#"<div class="x"><span>kept</span></div><b>bold</b>"#,
            r#"{ elements = {
                { "div, span", function(el) el:remove_and_keep_content() end },
                { "b", function(el) el:set_tag_name("strong") end },
            } }"#,
        )
        .unwrap();
        assert_eq!(out, "kept<strong>bold</strong>");
    }

    #[test]
    fn comments_and_text_have_handlers() {
        let out = rewrite_with(
            "a<!-- hidden -->b<!-- x -->",
            r#"{ comments = function(c)
                if c.text == " x " then c:set_text(" y ") else c:remove() end
            end }"#,
        )
        .unwrap();
        assert_eq!(out, "ab<!-- y -->");

        let out = rewrite_with(
            "<p>Hello &amp; bye</p>",
            r#"{ text = function(t) t:replace(t.text:upper(), "html") end }"#,
        )
        .unwrap();
        assert_eq!(out, "<p>HELLO &AMP; BYE</p>");
    }

    #[test]
    fn handler_errors_reach_the_caller() {
        let err = rewrite_with(
            "<p>x</p>",
            r#"{ elements = { { "p", function(el) error("boom") end } } }"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn objects_cannot_be_used_after_their_handler_returns() {
        let lua = lua();
        let err = eval::<()>(
            &lua,
            r#"
            local kept
            kiki.html.rewrite("<p>x</p>", { elements = { { "p", function(el) kept = el end } } })
            kept:remove()
            "#,
        )
        .unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn bad_handlers_are_errors() {
        let cases = [
            (
                r#"{ elements = { { "p[", function() end } } }"#,
                "invalid selector",
            ),
            (r#"{ elements = { { "p" } } }"#, "must be a function"),
            (
                r#"{ elements = { "p" } }"#,
                "must be a {selector, handler} pair",
            ),
            (
                r#"{ elements = { { "p", function(el) el:set_attribute("a b", "x") end } } }"#,
                "invalid attribute name",
            ),
            (
                r#"{ comments = function(c) c:set_text("-->") end }"#,
                "invalid comment text",
            ),
        ];
        for (handlers, message) in cases {
            let err = rewrite_with("<p>x<!-- c --></p>", handlers).unwrap_err();
            assert!(err.to_string().contains(message), "{handlers}: {err}");
        }
    }

    #[test]
    fn too_many_selectors_are_an_error() {
        let handlers = format!(
            r#"{{ elements = (function()
                local t = {{}}
                for i = 1, {} do t[i] = {{ "p", function() end }} end
                return t
            end)() }}"#,
            MAX_SELECTORS + 1
        );
        let err = rewrite_with("<p>x</p>", &handlers).unwrap_err();
        assert!(
            err.to_string().contains("too many element handlers"),
            "{err}"
        );
    }

    #[test]
    fn output_is_capped() {
        let err = rewrite_with(
            &"<p>x</p>".repeat(64),
            &format!(
                r#"{{ elements = {{ {{ "p", function(el) el:append(string.rep("x", {})) end }} }} }}"#,
                HTML_OUTPUT_LIMIT_BYTES / 16
            ),
        )
        .unwrap_err();
        assert!(err.to_string().contains("longer than"), "{err}");
    }

    #[test]
    fn escape_and_unescape() {
        let lua = lua();
        let (escaped, unescaped): (String, String) = eval(
            &lua,
            r#"return kiki.html.escape([[<a href="x">'&']]), kiki.html.unescape("&lt;b&gt; &amp;amp; &#x41;")"#,
        )
        .unwrap();
        assert_eq!(escaped, "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;");
        assert_eq!(unescaped, "<b> &amp; A");
    }

    #[test]
    fn rewrites_can_nest() {
        let out = rewrite_with(
            r#"<p data-html="&lt;b&gt;x&lt;/b&gt;">y</p>"#,
            r#"{ elements = { { "p", function(el)
                local inner = kiki.html.rewrite(el:get_attribute("data-html"), { elements = {
                    { "b", function(b) b:set_tag_name("i") end },
                } })
                el:remove_attribute("data-html")
                el:set_inner_content(inner, "html")
            end } } }"#,
        )
        .unwrap();
        assert_eq!(out, "<p><i>x</i></p>");
    }
}
