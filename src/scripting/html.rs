//! HTML as plugins parse and rewrite it, through the `html` interface WebAssembly plugins
//! import, with `lol_html`, the streaming rewriter Kiki uses for its own HTML.
//!
//! # Two passes
//!
//! `lol_html` calls a handler for each element as it parses, but the server cannot call
//! into a plugin while the plugin is calling the server. A rewrite therefore takes two
//! passes over the HTML: [`select`] returns the elements a CSS selector matches, in
//! document order, and [`rewrite`] parses the same HTML with the same selector again,
//! applying the plugin's edits to the elements by their index in what [`select`] returned.
//! The second pass matches the same elements in the same order as the first, since nothing
//! a rewrite inserts is parsed, and a removed element's descendants are still matched.
//!
//! [`strip`] instead rewrites the HTML in one pass, for plugins whose edits depend on what
//! they find in attribute values only: it removes or unwraps elements by their tag names
//! alone, as rules the plugin gives say, and takes every attribute off the elements it
//! keeps, returning them for the plugin to check. The plugin writes back the ones it keeps
//! itself, into the HTML [`strip`] returns, where markers show it the start tags.
//!
//! # Packing
//!
//! What [`select`] returns crosses into the plugin's memory, where every string and list
//! is a separate allocation, made by calling into the plugin. Its text (tag names,
//! attribute names and values) is therefore packed into one string, which the elements
//! and attributes refer to by [`Span`]s, so that the result takes three allocations
//! however many elements there are.
//!
//! # Attribute values
//!
//! `lol_html` hands over attribute values as they appear in the source, character
//! references and all, and writes values back escaping only double quotes. Plugins instead
//! see decoded values, which [`EditOp::SetAttribute`] escapes again, so that a value a
//! plugin sets is the value a browser reads back. A plugin that checks a value and then
//! sets it again therefore writes exactly what it checked, even where [`decode`] decodes a
//! value differently from a browser.
//!
//! # Resource limits
//!
//! The rewriter's buffers live outside the plugin's memory, so its memory limit does not
//! see them. They are bounded here instead: the rewriter may use at most
//! [`HTML_MEMORY_LIMIT_BYTES`] for its buffers, and what a call returns may take at most
//! [`HTML_OUTPUT_LIMIT_BYTES`], or the `max-bytes` the plugin gives, whichever is less.
//! Calls check the plugin's time budget as they go, through the `expired` callback they
//! are given.

use lol_html::html_content::{Comment, ContentType, Element};
use lol_html::{
    DocumentContentHandlers, ElementContentHandlers, HtmlRewriter, MemorySettings, Selector,
    Settings,
};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt::Write;

/// Most memory the rewriter may use for its buffers during one pass.
pub const HTML_MEMORY_LIMIT_BYTES: usize = 4 * 1024 * 1024;

/// Most memory what a call returns may take in the plugin's memory.
pub const HTML_OUTPUT_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// What an [`Element`] takes in the plugin's memory, as the component model lays out the
/// `element` record: two spans of two `u32`s, and an enum padded to four bytes.
const ELEMENT_BYTES: usize = 20;

/// What an [`Attribute`] takes in the plugin's memory: two spans of two `u32`s.
const ATTRIBUTE_BYTES: usize = 16;

/// How much HTML each pass parses between checks of the time budget.
const WRITE_CHUNK_BYTES: usize = 64 * 1024;

/// How many elements each pass handles between checks of the time budget.
const ELEMENTS_PER_CHECK: usize = 64;

/// Why a call failed.
#[derive(Debug, PartialEq, Eq)]
pub enum HtmlError {
    /// A message for the plugin.
    Failed(String),
    /// The plugin's time budget ran out.
    Expired,
}

impl HtmlError {
    fn failed(message: impl std::fmt::Display) -> Self {
        HtmlError::Failed(message.to_string())
    }
}

/// Where a piece of [`Selected::text`] starts, in bytes, and how long it is; or which run
/// of [`Selected::attributes`] an element's attributes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub len: u32,
}

/// The namespace an element is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    Html,
    Svg,
    MathMl,
}

/// An element [`select`] matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedElement {
    pub tag_name: Span,
    pub namespace: Namespace,
    pub attributes: Span,
}

/// An attribute of a [`SelectedElement`]: its name, lowercase, and its decoded value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attribute {
    pub name: Span,
    pub value: Span,
}

/// The elements [`select`] matched, in document order.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Selected {
    pub text: String,
    pub elements: Vec<SelectedElement>,
    pub attributes: Vec<Attribute>,
}

/// Where an insertion puts its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Before,
    After,
    Prepend,
    Append,
    /// In place of the element's content.
    Inner,
    /// In place of the element.
    Replace,
}

/// A change to an element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOp {
    /// Remove the element and its content.
    Remove,
    /// Remove the element's tags, keeping its content.
    Unwrap,
    /// Set an attribute to a decoded value.
    SetAttribute(String, String),
    RemoveAttribute(String),
    SetTagName(String),
    /// Insert content, as HTML if the flag is set, or else as text.
    Insert(Place, String, bool),
}

/// A change to the element at index `element` of what [`select`] returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub element: u32,
    pub op: EditOp,
}

/// What [`strip`] does with each element, by its tag name, lowercase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StripRules {
    /// Elements removed with their content.
    pub drop: HashSet<String>,
    /// Elements kept, without their attributes. Every other element is unwrapped: its tags
    /// are removed and its content kept.
    pub keep: HashSet<String>,
    /// Whether elements in the SVG and MathML namespaces are removed with their content,
    /// whatever their names.
    pub drop_foreign: bool,
    /// Whether comments are removed.
    pub remove_comments: bool,
}

/// What [`strip`] returns.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stripped {
    /// The stripped HTML, with [`STRIP_MARKER`] and the element's index in `elements`, in
    /// decimal, and [`STRIP_MARKER`] again, just before each kept element's start tag.
    pub html: String,
    /// The elements kept, with the attributes taken off them, in document order.
    pub elements: Selected,
}

/// The character around the index of an element [`strip`] kept, in front of its start tag.
/// [`strip`] replaces it with U+FFFD wherever the HTML holds it, as browsers do, so that it
/// marks start tags alone.
pub const STRIP_MARKER: char = '\0';

/// The error a content handler returns to stop the rewriter. What went wrong is kept
/// aside, in [`Pass::failure`].
#[derive(Debug, thiserror::Error)]
#[error("rewrite aborted")]
struct Aborted;

/// The state a pass's handlers share.
struct Pass<'a> {
    expired: &'a dyn Fn() -> bool,
    /// How many elements the pass has handled.
    seen: Cell<usize>,
    /// The first failure, which stopped the rewriter.
    failure: RefCell<Option<HtmlError>>,
}

impl<'a> Pass<'a> {
    fn new(expired: &'a dyn Fn() -> bool) -> Self {
        Pass {
            expired,
            seen: Cell::new(0),
            failure: RefCell::new(None),
        }
    }

    /// Stop the rewriter with `failure`.
    fn fail(&self, failure: HtmlError) -> lol_html::HandlerResult {
        self.failure.borrow_mut().get_or_insert(failure);
        Err(Box::new(Aborted))
    }

    /// Count an element, and stop the rewriter if the time budget has run out. Returns
    /// the element's index.
    fn next_element(&self) -> Result<usize, Box<Aborted>> {
        let index = self.seen.get();
        self.seen.set(index + 1);
        if index.is_multiple_of(ELEMENTS_PER_CHECK) && (self.expired)() {
            self.failure.borrow_mut().get_or_insert(HtmlError::Expired);
            return Err(Box::new(Aborted));
        }
        Ok(index)
    }

    /// Run `rewriter` over `html`, in chunks, checking the time budget between them.
    fn run<O: lol_html::OutputSink>(
        &self,
        mut rewriter: HtmlRewriter<'_, O>,
        html: &str,
    ) -> Result<(), HtmlError> {
        let mut result = Ok(());
        for chunk in html.as_bytes().chunks(WRITE_CHUNK_BYTES) {
            if (self.expired)() {
                return Err(HtmlError::Expired);
            }
            result = rewriter.write(chunk);
            if result.is_err() {
                break;
            }
        }
        let result = result.and_then(|()| rewriter.end());
        if let Some(failure) = self.failure.take() {
            return Err(failure);
        }
        result.map_err(|e| match e {
            lol_html::errors::RewritingError::MemoryLimitExceeded(_) => {
                HtmlError::failed("the HTML needs too much memory to parse")
            }
            other => HtmlError::failed(format!("unable to parse the HTML: {other}")),
        })
    }
}

fn parse_selector(selector: &str) -> Result<Selector, HtmlError> {
    selector
        .parse()
        .map_err(|e| HtmlError::failed(format!("invalid selector {selector:?}: {e}")))
}

fn settings<'h>(
    element_content_handlers: Vec<(Cow<'h, Selector>, ElementContentHandlers<'h>)>,
    document_content_handlers: Vec<DocumentContentHandlers<'h>>,
) -> Settings<'h, 'h> {
    Settings {
        element_content_handlers,
        document_content_handlers,
        memory_settings: MemorySettings {
            max_allowed_memory_usage: HTML_MEMORY_LIMIT_BYTES,
            ..MemorySettings::default()
        },
        ..Settings::new()
    }
}

/// The elements of `html` that `selector` matches, taking at most `max_bytes` (and at most
/// [`HTML_OUTPUT_LIMIT_BYTES`]) of the plugin's memory. `expired` says whether the
/// plugin's time budget has run out.
///
/// # Errors
///
/// Fails if the selector is invalid, the HTML cannot be parsed within the rewriter's
/// memory limit, or the elements would take more than the bytes allowed; or with
/// [`HtmlError::Expired`] once `expired` says so.
///
/// # Examples
///
/// ```
/// use kiki_rss::scripting::html::select;
///
/// let selected = select(r#"<p><a HREF="/a?x=1&amp;y=2">a</a></p>"#, "a", 1024, &|| false).unwrap();
/// let a = selected.elements[0];
/// assert_eq!(selected.text(a.tag_name), "a");
/// let href = selected.attributes[a.attributes.start as usize];
/// assert_eq!(selected.text(href.name), "href");
/// assert_eq!(selected.text(href.value), "/a?x=1&y=2");
/// ```
pub fn select(
    html: &str,
    selector: &str,
    max_bytes: u32,
    expired: &dyn Fn() -> bool,
) -> Result<Selected, HtmlError> {
    let selector = parse_selector(selector)?;
    let limit = (max_bytes as usize).min(HTML_OUTPUT_LIMIT_BYTES);
    let pass = Pass::new(expired);
    let selected = RefCell::new(Selected::default());

    let handler = ElementContentHandlers::default().element(|el: &mut Element| {
        pass.next_element()?;
        let mut selected = selected.borrow_mut();
        selected.push(el, &el.tag_name(), namespace(el));
        if selected.bytes() > limit {
            return pass.fail(HtmlError::failed(format!(
                "the elements selected take more than {limit} bytes"
            )));
        }
        Ok(())
    });

    let rewriter = HtmlRewriter::new(
        settings(vec![(Cow::Owned(selector), handler)], vec![]),
        |_: &[u8]| {},
    );
    pass.run(rewriter, html)?;
    Ok(selected.into_inner())
}

/// The namespace `el` is in.
fn namespace(el: &Element) -> Namespace {
    match el.namespace_uri() {
        "http://www.w3.org/2000/svg" => Namespace::Svg,
        "http://www.w3.org/1998/Math/MathML" => Namespace::MathMl,
        _ => Namespace::Html,
    }
}

/// Append `s` to `text`, returning where it went. `text` is kept below
/// [`HTML_OUTPUT_LIMIT_BYTES`], so offsets fit in a `u32`.
fn push_text(text: &mut String, s: &str) -> Span {
    let start = text.len() as u32;
    text.push_str(s);
    Span {
        start,
        len: s.len() as u32,
    }
}

impl Selected {
    /// Add `el`, whose tag name and namespace are `tag_name` and `namespace`, with its
    /// attributes, their values decoded.
    fn push(&mut self, el: &Element, tag_name: &str, namespace: Namespace) {
        let Selected {
            text,
            elements,
            attributes,
        } = self;
        let tag_name = push_text(text, tag_name);
        let first = attributes.len();
        for attribute in el.attributes() {
            let name = push_text(text, &attribute.name());
            let value = push_text(text, &decode(&attribute.value()));
            attributes.push(Attribute { name, value });
        }
        elements.push(SelectedElement {
            tag_name,
            namespace,
            attributes: Span {
                start: first as u32,
                len: (attributes.len() - first) as u32,
            },
        });
    }

    /// How many bytes these take in the plugin's memory.
    fn bytes(&self) -> usize {
        self.text.len()
            + self.elements.len() * ELEMENT_BYTES
            + self.attributes.len() * ATTRIBUTE_BYTES
    }

    /// The piece of [`Selected::text`] at `span`.
    pub fn text(&self, span: Span) -> &str {
        let start = span.start as usize;
        self.text
            .get(start..start + span.len as usize)
            .unwrap_or_default()
    }
}

/// `html` with `edits` applied to the elements `selector` matches, and its comments
/// removed if `remove_comments` is set. The result may be at most `max_bytes` long (and
/// at most [`HTML_OUTPUT_LIMIT_BYTES`]). `expired` says whether the plugin's time budget
/// has run out.
///
/// # Errors
///
/// Fails if the selector is invalid, an edit sets an invalid attribute or tag name, an
/// edit is for an element past the last one matched, the HTML cannot be parsed within the
/// rewriter's memory limit, or the result is too long; or with [`HtmlError::Expired`] once
/// `expired` says so.
///
/// # Examples
///
/// ```
/// use kiki_rss::scripting::html::{rewrite, Edit, EditOp};
///
/// let edits = vec![
///     Edit { element: 0, op: EditOp::Unwrap },
///     Edit { element: 1, op: EditOp::SetAttribute("rel".into(), "nofollow".into()) },
/// ];
/// let out = rewrite(r#"<div><a href="/">a</a></div>"#, "*", edits, false, 1024, &|| false);
/// assert_eq!(out.unwrap(), r#"<a href="/" rel="nofollow">a</a>"#);
/// ```
pub fn rewrite(
    html: &str,
    selector: &str,
    mut edits: Vec<Edit>,
    remove_comments: bool,
    max_bytes: u32,
    expired: &dyn Fn() -> bool,
) -> Result<String, HtmlError> {
    let selector = parse_selector(selector)?;
    let limit = (max_bytes as usize).min(HTML_OUTPUT_LIMIT_BYTES);
    let pass = Pass::new(expired);

    // Stable, so that each element's edits keep their order.
    edits.sort_by_key(|e| e.element);
    let edits = RefCell::new(edits.into_iter().peekable());

    let handler = ElementContentHandlers::default().element(|el: &mut Element| {
        let index = pass.next_element()?;
        let mut edits = edits.borrow_mut();
        while let Some(edit) = edits.next_if(|e| e.element as usize == index) {
            if let Err(e) = apply(el, edit.op) {
                return pass.fail(e);
            }
        }
        Ok(())
    });
    let mut document = DocumentContentHandlers::default();
    if remove_comments {
        document = document.comments(|c: &mut Comment| {
            c.remove();
            Ok(())
        });
    }

    let mut out = Vec::new();
    let overflowed = Cell::new(false);
    let rewriter = HtmlRewriter::new(
        settings(vec![(Cow::Owned(selector), handler)], vec![document]),
        |chunk: &[u8]| {
            if overflowed.get() || out.len() + chunk.len() > limit {
                overflowed.set(true);
            } else {
                out.extend_from_slice(chunk);
            }
        },
    );
    pass.run(rewriter, html)?;

    if let Some(edit) = edits.borrow_mut().next() {
        return Err(HtmlError::failed(format!(
            "an edit is for element {}, but the selector matched only {}",
            edit.element,
            pass.seen.get()
        )));
    }
    if overflowed.get() {
        return Err(HtmlError::failed(format!(
            "the rewritten HTML is longer than {limit} bytes"
        )));
    }
    // The output is the input's UTF-8, with the plugin's UTF-8 inserted.
    String::from_utf8(out)
        .map_err(|e| HtmlError::failed(format!("the rewritten HTML is not UTF-8: {e}")))
}

/// `html` rewritten in one pass by `rules`: elements removed with their content, kept
/// without their attributes, or unwrapped, and comments removed if the rules say so. The
/// elements kept are returned with the attributes taken off them, values decoded, and the
/// HTML marks each one's start tag with its index, as [`Stripped::html`] describes.
///
/// What is returned may take at most `max_bytes` (and at most
/// [`HTML_OUTPUT_LIMIT_BYTES`]) of the plugin's memory, and `expired` says whether the
/// plugin's time budget has run out.
///
/// # Errors
///
/// Fails if the HTML cannot be parsed within the rewriter's memory limit, or what is
/// returned would take more than the bytes allowed; or with [`HtmlError::Expired`] once
/// `expired` says so.
///
/// # Examples
///
/// ```
/// use kiki_rss::scripting::html::{strip, StripRules};
///
/// let rules = StripRules {
///     drop: ["script".to_string()].into(),
///     keep: ["a".to_string()].into(),
///     ..StripRules::default()
/// };
/// let html = r#"<div><a href="/a">a</a><script>x()</script></div>"#;
/// let stripped = strip(html, &rules, 1024, &|| false).unwrap();
/// assert_eq!(stripped.html, "\x000\x00<a>a</a>");
/// let a = stripped.elements.elements[0];
/// assert_eq!(stripped.elements.text(a.tag_name), "a");
/// ```
pub fn strip(
    html: &str,
    rules: &StripRules,
    max_bytes: u32,
    expired: &dyn Fn() -> bool,
) -> Result<Stripped, HtmlError> {
    let limit = (max_bytes as usize).min(HTML_OUTPUT_LIMIT_BYTES);
    let html: Cow<str> = if html.contains(STRIP_MARKER) {
        Cow::Owned(html.replace(STRIP_MARKER, "\u{FFFD}"))
    } else {
        Cow::Borrowed(html)
    };
    let pass = Pass::new(expired);
    let selected = RefCell::new(Selected::default());
    // How many bytes `selected` takes, for the output sink, which can't borrow it.
    let selected_bytes = Cell::new(0);
    let written = Cell::new(0);
    let marker = RefCell::new(String::new());

    let handler = ElementContentHandlers::default().element(|el: &mut Element| {
        pass.next_element()?;
        let tag_name = el.tag_name();
        let namespace = namespace(el);
        if (rules.drop_foreign && namespace != Namespace::Html) || rules.drop.contains(&tag_name) {
            el.remove();
            return Ok(());
        }
        if !rules.keep.contains(&tag_name) {
            el.remove_and_keep_content();
            return Ok(());
        }
        let mut selected = selected.borrow_mut();
        let index = selected.elements.len();
        selected.push(el, &tag_name, namespace);
        selected_bytes.set(selected.bytes());
        if selected_bytes.get() + written.get() > limit {
            return pass.fail(HtmlError::failed(format!(
                "the stripped HTML takes more than {limit} bytes"
            )));
        }
        let first = selected.attributes.len() - el.attributes().len();
        for attribute in selected.attributes.get(first..).unwrap_or_default() {
            el.remove_attribute(selected.text(attribute.name));
        }
        let mut marker = marker.borrow_mut();
        marker.clear();
        // Writing to a `String` can't fail.
        let _ = write!(marker, "{STRIP_MARKER}{index}{STRIP_MARKER}");
        el.before(&marker, ContentType::Html);
        Ok(())
    });
    let mut document = DocumentContentHandlers::default();
    if rules.remove_comments {
        document = document.comments(|c: &mut Comment| {
            c.remove();
            Ok(())
        });
    }

    let mut out = Vec::new();
    let overflowed = Cell::new(false);
    let rewriter = HtmlRewriter::new(
        settings(
            vec![(Cow::Owned(parse_selector("*")?), handler)],
            vec![document],
        ),
        |chunk: &[u8]| {
            if overflowed.get() || out.len() + chunk.len() + selected_bytes.get() > limit {
                overflowed.set(true);
            } else {
                out.extend_from_slice(chunk);
                written.set(out.len());
            }
        },
    );
    pass.run(rewriter, &html)?;
    if overflowed.get() {
        return Err(HtmlError::failed(format!(
            "the stripped HTML takes more than {limit} bytes"
        )));
    }
    // The output is the input's UTF-8, with the markers inserted.
    let html = String::from_utf8(out)
        .map_err(|e| HtmlError::failed(format!("the stripped HTML is not UTF-8: {e}")))?;
    Ok(Stripped {
        html,
        elements: selected.into_inner(),
    })
}

/// The content of the first element of `html` that `selector` matches, as HTML: what is
/// between its start tag and its end tag, or `None` if nothing matches. An element whose
/// end tag is missing has the rest of the HTML as its content, and one that cannot have
/// content, such as `<img>`, has none. The content may be at most `max_bytes` long (and at
/// most [`HTML_OUTPUT_LIMIT_BYTES`]). `expired` says whether the plugin's time budget has
/// run out.
///
/// # Errors
///
/// Fails if the selector is invalid, the HTML cannot be parsed within the rewriter's
/// memory limit, or the content is too long; or with [`HtmlError::Expired`] once `expired`
/// says so.
///
/// # Examples
///
/// ```
/// use kiki_rss::scripting::html::inner;
///
/// let html = r#"<nav>x</nav><article><p>One</p><article>Two</article></article>"#;
/// let content = inner(html, "article", 1024, &|| false).unwrap();
/// assert_eq!(content.as_deref(), Some("<p>One</p><article>Two</article>"));
/// assert_eq!(inner(html, "main", 1024, &|| false).unwrap(), None);
/// ```
pub fn inner(
    html: &str,
    selector: &str,
    max_bytes: u32,
    expired: &dyn Fn() -> bool,
) -> Result<Option<String>, HtmlError> {
    /// Where the output sink is: before the element's content, in it, or past it.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum At {
        Before,
        In,
        After,
    }

    let selector = parse_selector(selector)?;
    let limit = (max_bytes as usize).min(HTML_OUTPUT_LIMIT_BYTES);
    // The content is found between two markers, so the HTML may not hold any.
    let html: Cow<str> = if html.contains(STRIP_MARKER) {
        Cow::Owned(html.replace(STRIP_MARKER, "\u{FFFD}"))
    } else {
        Cow::Borrowed(html)
    };
    let marker = STRIP_MARKER.to_string();
    let pass = Pass::new(expired);
    let found = Cell::new(false);

    let handler = ElementContentHandlers::default().element(|el: &mut Element| {
        pass.next_element()?;
        if found.replace(true) || !el.can_have_content() {
            return Ok(());
        }
        el.prepend(&marker, ContentType::Html);
        if let Some(end_tag) = el.end_tag_handlers() {
            let marker = marker.clone();
            end_tag.push(Box::new(move |end| {
                end.before(&marker, ContentType::Html);
                Ok(())
            }));
        }
        Ok(())
    });

    let at = Cell::new(At::Before);
    let mut out = Vec::new();
    let overflowed = Cell::new(false);
    let rewriter = HtmlRewriter::new(
        settings(vec![(Cow::Owned(selector), handler)], vec![]),
        |mut chunk: &[u8]| {
            // A marker is one byte, so it is never split across chunks.
            if at.get() == At::Before {
                match chunk.iter().position(|&b| b == 0) {
                    Some(i) => {
                        at.set(At::In);
                        chunk = chunk.get(i + 1..).unwrap_or_default();
                    }
                    None => return,
                }
            }
            if at.get() == At::In {
                if let Some(i) = chunk.iter().position(|&b| b == 0) {
                    at.set(At::After);
                    chunk = chunk.get(..i).unwrap_or_default();
                }
                if overflowed.get() || out.len() + chunk.len() > limit {
                    overflowed.set(true);
                } else {
                    out.extend_from_slice(chunk);
                }
            }
        },
    );
    pass.run(rewriter, &html)?;
    if !found.get() {
        return Ok(None);
    }
    if overflowed.get() {
        return Err(HtmlError::failed(format!(
            "the element's content is longer than {limit} bytes"
        )));
    }
    // An element that cannot have content takes no markers.
    if at.get() == At::Before {
        return Ok(Some(String::new()));
    }
    // The output is the input's UTF-8, cut at markers, which are ASCII.
    String::from_utf8(out)
        .map(Some)
        .map_err(|e| HtmlError::failed(format!("the element's content is not UTF-8: {e}")))
}

fn apply(el: &mut Element, op: EditOp) -> Result<(), HtmlError> {
    match op {
        EditOp::Remove => el.remove(),
        EditOp::Unwrap => el.remove_and_keep_content(),
        EditOp::SetAttribute(name, value) => el
            .set_attribute(&name, &escape_attribute(&value))
            .map_err(|e| HtmlError::failed(format!("invalid attribute name {name:?}: {e}")))?,
        EditOp::RemoveAttribute(name) => el.remove_attribute(&name),
        EditOp::SetTagName(name) => el
            .set_tag_name(&name)
            .map_err(|e| HtmlError::failed(format!("invalid tag name {name:?}: {e}")))?,
        EditOp::Insert(place, content, html) => {
            let ct = if html {
                ContentType::Html
            } else {
                ContentType::Text
            };
            match place {
                Place::Before => el.before(&content, ct),
                Place::After => el.after(&content, ct),
                Place::Prepend => el.prepend(&content, ct),
                Place::Append => el.append(&content, ct),
                Place::Inner => el.set_inner_content(&content, ct),
                Place::Replace => el.replace(&content, ct),
            }
        }
    }
    Ok(())
}

/// `s` with its character references decoded, at most `max_bytes` long.
///
/// # Errors
///
/// Fails if the decoded text is longer than `max_bytes`.
///
/// # Examples
///
/// ```
/// use kiki_rss::scripting::html::unescape;
///
/// assert_eq!(unescape("&lt;b&gt; &amp;amp; &#x41;", 64).unwrap(), "<b> &amp; A");
/// ```
pub fn unescape(s: &str, max_bytes: u32) -> Result<String, HtmlError> {
    let decoded = decode(s);
    if decoded.len() > max_bytes as usize {
        return Err(HtmlError::failed(format!(
            "the unescaped text is longer than {max_bytes} bytes"
        )));
    }
    Ok(decoded.into_owned())
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

/// Decode the character references in `s` the way a browser decodes an attribute value.
///
/// Numeric references are decoded with or without their terminating `;`, with the code
/// points the HTML standard replaces mapped as it says. Named references are decoded when
/// they end in `;`; the few legacy names a browser also decodes without the `;` are left
/// as they are.
pub fn decode(s: &str) -> Cow<'_, str> {
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
#[path = "html_tests.rs"]
mod tests;
