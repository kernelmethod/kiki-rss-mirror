//! Write [Kiki](https://github.com/kernelmethod/kiki-rss) plugins in Rust.
//!
//! A Kiki plugin is a WebAssembly component targeting the `plugin` world of
//! `wit/kiki-plugin.wit`. This crate generates the bindings for it and wraps them in the
//! [`Plugin`] trait and the [`plugin`] attribute: implement [`Plugin`] to make the plugin
//! from its config, and put `#[plugin]` on an `impl` block holding its handlers, each a
//! method marked with the event it handles.
//!
//! ```ignore
//! use kiki_plugin::{plugin, Entry, Plugin};
//!
//! struct Shout;
//!
//! impl Plugin for Shout {
//!     fn new(_config: &str) -> Result<Self, String> {
//!         Ok(Shout)
//!     }
//! }
//!
//! #[plugin]
//! impl Shout {
//!     #[on(entry.ingest)]
//!     fn shout(&mut self, mut entry: Entry) -> Option<Entry> {
//!         entry.title = entry.title.to_uppercase();
//!         Some(entry)
//!     }
//! }
//! ```
//!
//! Build it as a `cdylib` for `wasm32-wasip2`:
//!
//! ```text
//! cargo build --release --target wasm32-wasip2
//! ```
//!
//! and install `target/wasm32-wasip2/release/<name>.wasm` as the plugin's `plugin.wasm`,
//! next to a `manifest.toml` with `engine = "wasm"`.
//!
//! Of WASI, Kiki gives plugins only randomness and clocks, so `HashMap`'s default hasher,
//! `std::time::SystemTime::now` and `std::time::Instant` work. Files, the network, the
//! environment and the standard streams are not available: `std::fs`, `std::env` and
//! `println!` trap. Use [`log`] to write to Kiki's log.

#![warn(missing_docs)]

pub use kiki_plugin_macros::plugin;
use std::any::Any;
use std::cell::RefCell;

/// The bindings generated from `kiki-plugin.wit`.
#[doc(hidden)]
// `html::rewrite`'s arguments take nine values in the generated code.
#[allow(clippy::too_many_arguments)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "plugin",
        pub_export_macro: true,
        export_macro_name: "export",
        default_bindings_module: "::kiki_plugin::bindings",
    });
}

pub use bindings::kiki::plugin::types::{
    ContentChange, DeleteFilter, Entry, EventKind, Feed, FeedEvent, FetchError, FetchSchedule,
    FetchSuccess, Level, Page, ScanOptions, ScanSummary,
};

/// Calls to the server: logging, the plugin's store, stored entries, feeds, timers and web
/// pages.
///
/// [`fetch_page`](host::fetch_page) needs the `network` permission in the plugin's
/// manifest, and fails unless Kiki's settings allow plugins to use the network
/// (`allow_network` under `[plugins]` in `kiki.toml`), which they don't by default.
pub mod host {
    pub use crate::bindings::kiki::plugin::host::*;

    #[cfg(feature = "serde")]
    /// Reads the value stored under `key`, deserialized from JSON.
    ///
    /// # Errors
    ///
    /// Fails if the server refuses the call, or the value doesn't deserialize as `T`.
    pub fn get<T: serde::de::DeserializeOwned>(key: &str) -> Result<Option<T>, String> {
        match store_get(key)? {
            None => Ok(None),
            Some(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| format!("store value {key:?}: {e}")),
        }
    }

    #[cfg(feature = "serde")]
    /// Stores `value`, serialized as JSON, under `key`.
    ///
    /// # Errors
    ///
    /// Fails if `value` can't be serialized, or the server refuses the call.
    pub fn set<T: serde::Serialize>(key: &str, value: &T) -> Result<(), String> {
        let text = serde_json::to_string(value).map_err(|e| e.to_string())?;
        store_set(key, Some(&text))
    }
}

/// Regular expressions, compiled and matched by the server.
///
/// Matching runs as native code in the server, so it is faster than a regex library built
/// into the plugin, and leaves the plugin smaller: the `regex` crate adds about a
/// megabyte to a WebAssembly plugin. Flags are letters: `i` (case-insensitive), `m`
/// (multi-line), `s` (`.` matches a newline), `x` (ignore whitespace) and `U` (swap
/// greed). A plugin may have 128 distinct patterns, with their flags, alive at once, alone
/// or in a [`RegexSet`](regex::RegexSet): compiling one it has alive already shares it,
/// and dropping the last regex or set holding a pattern frees its place. A set matches
/// each of its patterns in one call, saving a call to the server per pattern.
///
/// ```ignore
/// use kiki_plugin::regex::{Regex, RegexSet};
///
/// let re = Regex::compile(r"\bkiki\b", "i")?;
/// assert!(re.is_match("Hello, Kiki!"));
/// assert_eq!(re.find("a kiki", 0), Some((2, 6)));
///
/// let set = RegexSet::compile(&[("rust".into(), "i".into()), ("^go".into(), "".into())])?;
/// assert_eq!(set.matches("Rust and go"), [0]);
/// ```
pub mod regex {
    pub use crate::bindings::kiki::plugin::regex::{Regex, RegexSet};
}

/// HTML, parsed and rewritten by the server.
///
/// Parsing runs as native code in the server, with `lol_html`, the streaming rewriter Kiki
/// uses for its own HTML, so it is faster than a parser built into the plugin, and leaves
/// the plugin smaller: `lol_html` adds about 600 KB to a WebAssembly plugin.
///
/// The server cannot call into the plugin while the plugin is calling it, so a rewrite
/// takes two passes. [`select`](html::select) returns the elements a CSS selector matches,
/// in document order, with their attributes; the plugin records what to do with them in
/// [`Edits`](html::Edits), by their index; and [`rewrite`](html::rewrite) parses the same
/// HTML with the same selector again, applying the edits. A removed element's descendants
/// are still matched, and nothing a rewrite inserts is parsed, so the second pass sees the
/// elements the first saw.
///
/// Attribute values are decoded, as a browser decodes them, and the values set are
/// escaped, so that a value read and set again is written out as what was read.
///
/// ```ignore
/// use kiki_plugin::html::{self, Edits, Place};
///
/// let content = r#"<p>a <a href="https://example.com/" onclick="x()">link</a></p>"#;
/// let mut edits = Edits::new();
/// for a in html::select(content, "a[href]")?.iter() {
///     if a.attribute("href").is_some_and(|href| href.starts_with("https://")) {
///         edits.remove_attribute(a.index(), "onclick");
///         edits.set_attribute(a.index(), "rel", "noopener");
///         edits.insert_text(a.index(), Place::After, " (external)");
///     }
/// }
/// assert_eq!(
///     html::rewrite(content, "a[href]", &edits)?,
///     r#"<p>a <a href="https://example.com/" rel="noopener">link</a> (external)</p>"#
/// );
/// ```
///
/// What the server returns goes into the plugin's memory, and a plugin out of memory for
/// it would trap. Each call therefore first finds out how much room the plugin's memory has
/// for the result, and the server fails the call rather than return more, or more than
/// [`MAX_RESULT_BYTES`](html::MAX_RESULT_BYTES).
pub mod html {
    use crate::bindings::kiki::plugin::html as wit;
    pub use wit::{Namespace, Place, StripRules};

    /// Most memory the result of a call may take in the plugin's memory: the elements
    /// [`select`] returns, the HTML [`rewrite`] returns, what [`strip`] returns or the text
    /// [`unescape`] returns.
    pub const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;

    /// Memory kept free, beyond a result's own size, for the allocations that hold it.
    const SLACK_BYTES: usize = 256;

    /// How many bytes, up to `want` (and [`MAX_RESULT_BYTES`]), the plugin's memory has
    /// room for in one piece: finding it out leaves that much free, so the server can
    /// return that much into it.
    fn room(want: usize) -> u32 {
        let mut size = want.min(MAX_RESULT_BYTES);
        loop {
            if size == 0
                || Vec::<u8>::new()
                    .try_reserve_exact(size + SLACK_BYTES)
                    .is_ok()
            {
                return size as u32;
            }
            size -= size / 8 + 1;
        }
    }

    /// The elements a selector matched, in document order.
    pub struct Elements(wit::Elements);

    impl Elements {
        /// How many elements were matched.
        pub fn len(&self) -> usize {
            self.0.elements.len()
        }

        /// Whether no element was matched.
        pub fn is_empty(&self) -> bool {
            self.0.elements.is_empty()
        }

        /// The element at `index`, if any.
        pub fn get(&self, index: u32) -> Option<Element<'_>> {
            let raw = self.0.elements.get(index as usize)?;
            Some(Element {
                elements: self,
                index,
                raw,
            })
        }

        /// The elements, in document order.
        pub fn iter(&self) -> impl ExactSizeIterator<Item = Element<'_>> + '_ {
            self.0
                .elements
                .iter()
                .enumerate()
                .map(|(index, raw)| Element {
                    elements: self,
                    index: index as u32,
                    raw,
                })
        }

        fn text(&self, span: wit::Span) -> &str {
            let start = span.start as usize;
            self.0
                .text
                .get(start..start + span.len as usize)
                .unwrap_or_default()
        }
    }

    /// An element [`select`] matched, or [`strip`] kept.
    #[derive(Clone, Copy)]
    pub struct Element<'a> {
        elements: &'a Elements,
        index: u32,
        raw: &'a wit::Element,
    }

    impl<'a> Element<'a> {
        /// The element's index among those matched, which [`Edits`] refer to it by.
        pub fn index(&self) -> u32 {
            self.index
        }

        /// The element's tag name, lowercase.
        pub fn tag_name(&self) -> &'a str {
            self.elements.text(self.raw.tag_name)
        }

        /// The namespace the element is in: HTML, or SVG or MathML for an element inside an
        /// `<svg>` or a `<math>`.
        pub fn namespace(&self) -> Namespace {
            self.raw.namespace
        }

        /// The element's attributes, as names, lowercase, and decoded values, in the order
        /// they appear. An element can have several attributes with the same name.
        pub fn attributes(&self) -> impl ExactSizeIterator<Item = (&'a str, &'a str)> + 'a {
            let elements = self.elements;
            let start = self.raw.attributes.start as usize;
            let len = self.raw.attributes.len as usize;
            elements
                .0
                .attributes
                .get(start..start + len)
                .unwrap_or_default()
                .iter()
                .map(move |a| (elements.text(a.name), elements.text(a.value)))
        }

        /// The value of the first attribute named `name`, ignoring case, if any.
        pub fn attribute(&self, name: &str) -> Option<&'a str> {
            self.attributes()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v)
        }
    }

    /// Changes to the elements [`select`] matched, for [`rewrite`] to make, each to the
    /// element at an index. An element's edits are made in the order they were added.
    #[derive(Default)]
    pub struct Edits {
        edits: Vec<wit::Edit>,
        remove_comments: bool,
        /// The bytes the edits add, for the room the result needs.
        payload: usize,
    }

    impl Edits {
        /// No edits.
        pub fn new() -> Self {
            Self::default()
        }

        /// How many edits there are.
        pub fn len(&self) -> usize {
            self.edits.len()
        }

        /// Whether there are none.
        pub fn is_empty(&self) -> bool {
            self.edits.is_empty()
        }

        fn push(&mut self, element: u32, op: wit::EditOp, payload: usize) -> &mut Self {
            self.edits.push(wit::Edit { element, op });
            self.payload += payload;
            self
        }

        /// Remove the element and its content.
        pub fn remove(&mut self, element: u32) -> &mut Self {
            self.push(element, wit::EditOp::Remove, 0)
        }

        /// Remove the element's tags, keeping its content.
        pub fn remove_and_keep_content(&mut self, element: u32) -> &mut Self {
            self.push(element, wit::EditOp::Unwrap, 0)
        }

        /// Set the attribute `name` to `value`, which is escaped as needed. An invalid name
        /// fails the rewrite.
        pub fn set_attribute(&mut self, element: u32, name: &str, value: &str) -> &mut Self {
            let op = wit::EditOp::SetAttribute((name.to_string(), value.to_string()));
            self.push(element, op, name.len() + value.len())
        }

        /// Remove the attribute `name`, every one if there are several.
        pub fn remove_attribute(&mut self, element: u32, name: &str) -> &mut Self {
            self.push(element, wit::EditOp::RemoveAttribute(name.to_string()), 0)
        }

        /// Rename the element. An invalid name fails the rewrite.
        pub fn set_tag_name(&mut self, element: u32, name: &str) -> &mut Self {
            let op = wit::EditOp::SetTagName(name.to_string());
            self.push(element, op, 2 * name.len())
        }

        /// Insert `text`, escaped, at `place`.
        pub fn insert_text(&mut self, element: u32, place: Place, text: &str) -> &mut Self {
            let op = wit::EditOp::InsertText((place, text.to_string()));
            self.push(element, op, text.len())
        }

        /// Insert `html`, as it is, at `place`.
        pub fn insert_html(&mut self, element: u32, place: Place, html: &str) -> &mut Self {
            let op = wit::EditOp::InsertHtml((place, html.to_string()));
            self.push(element, op, html.len())
        }

        /// Remove the HTML's comments too.
        pub fn remove_comments(&mut self) -> &mut Self {
            self.remove_comments = true;
            self
        }
    }

    /// The elements of `html` that `selector`, a CSS selector such as `"a[href], img"`,
    /// matches, in document order.
    ///
    /// # Errors
    ///
    /// Fails if the selector is invalid, the HTML needs more memory than the server allows
    /// to parse, or the elements need more room than the plugin's memory has.
    pub fn select(html: &str, selector: &str) -> Result<Elements, String> {
        // An element takes 20 bytes and an attribute 16, besides their text: up to nine
        // times what they take in the HTML, as the attributes of `<a b c d>` do.
        let max_bytes = room(html.len().saturating_mul(9).saturating_add(64));
        wit::select(html, selector, max_bytes).map(Elements)
    }

    /// `html` with `edits` made to the elements `selector` matches, as [`select`] returned
    /// them for the same `html` and `selector`.
    ///
    /// # Errors
    ///
    /// Fails if the selector is invalid, an edit sets an invalid attribute or tag name, an
    /// edit is for an element past the last one matched, the HTML needs more memory than
    /// the server allows to parse, or the result needs more room than the plugin's memory
    /// has.
    pub fn rewrite(html: &str, selector: &str, edits: &Edits) -> Result<String, String> {
        // Escaping makes text up to six times as long, and an attribute takes ` ="` too.
        let want = html
            .len()
            .saturating_add(edits.payload.saturating_mul(6))
            .saturating_add(edits.len().saturating_mul(4));
        let max_bytes = room(want);
        wit::rewrite(
            html,
            selector,
            &edits.edits,
            edits.remove_comments,
            max_bytes,
        )
    }

    /// `html` rewritten in one pass by `rules`: elements removed with their content, kept
    /// without their attributes, or unwrapped, by their tag names, and comments removed if
    /// the rules say so. The attributes taken off the elements kept are returned with them,
    /// for the plugin to check and write back as it goes through [`Stripped::pieces`].
    ///
    /// Where [`select`] and [`rewrite`] parse the HTML twice, this parses it once, and no
    /// edits cross back to Kiki: use it when what to do with an element depends on its tag
    /// name alone, and with an attribute on its value.
    ///
    /// # Errors
    ///
    /// Fails if the HTML needs more memory than the server allows to parse, or what is
    /// returned needs more room than the plugin's memory has.
    pub fn strip(html: &str, rules: &StripRules) -> Result<Stripped, String> {
        // As for `select`, and the HTML besides, which grows by a marker of at most 12
        // bytes for each start tag of at least 3.
        let max_bytes = room(html.len().saturating_mul(13).saturating_add(64));
        let stripped = wit::strip(html, rules, max_bytes)?;
        Ok(Stripped {
            html: stripped.html,
            elements: Elements(stripped.elements),
        })
    }

    /// HTML [`strip`] rewrote, and the elements it kept, with the attributes it took off
    /// them.
    pub struct Stripped {
        html: String,
        elements: Elements,
    }

    /// A piece of the HTML [`strip`] returned, in order: what to write out to rebuild it.
    pub enum Piece<'a> {
        /// HTML to write out as it is.
        Html(&'a str),
        /// A kept element's start tag, without attributes, split where they go. Write
        /// `open`, the attributes to keep, each after a space, and `close`; or nothing, to
        /// remove a void element such as `<img>`.
        StartTag {
            /// The element, with the attributes taken off it.
            element: Element<'a>,
            /// The `<` and the tag name.
            open: &'a str,
            /// The rest of the tag: `>` or `/>`, or as it was written if the element had
            /// no attributes. After attributes, `/>` needs a space before it.
            close: &'a str,
        },
    }

    impl Stripped {
        /// The elements kept, with the attributes taken off them, in document order.
        pub fn elements(&self) -> &Elements {
            &self.elements
        }

        /// The stripped HTML's length, which rebuilding it from [`Self::pieces`] adds the
        /// attributes kept to.
        pub fn len(&self) -> usize {
            self.html.len()
        }

        /// Whether the stripped HTML is empty.
        pub fn is_empty(&self) -> bool {
            self.html.is_empty()
        }

        /// The stripped HTML, in pieces, in order.
        pub fn pieces(&self) -> impl Iterator<Item = Piece<'_>> + '_ {
            let mut rest = self.html.as_str();
            let mut tag: Option<Piece<'_>> = None;
            std::iter::from_fn(move || {
                if let Some(tag) = tag.take() {
                    return Some(tag);
                }
                if rest.is_empty() {
                    return None;
                }
                let Some((before, after)) = rest.split_once('\0') else {
                    return Some(Piece::Html(std::mem::take(&mut rest)));
                };
                match self.start_tag(after) {
                    Some((piece, after_tag)) => {
                        tag = Some(piece);
                        rest = after_tag;
                    }
                    // Not a marker, which Kiki never writes: kept as text.
                    None => {
                        let (text, after) = rest.split_at(before.len() + 1);
                        rest = after;
                        return Some(Piece::Html(text));
                    }
                }
                if before.is_empty() {
                    return tag.take();
                }
                Some(Piece::Html(before))
            })
        }

        /// The start tag marked by `marked`, the HTML after a marker's first NUL, and the
        /// HTML after it.
        fn start_tag<'a>(&'a self, marked: &'a str) -> Option<(Piece<'a>, &'a str)> {
            let (index, tag) = marked.split_once('\0')?;
            let element = self.elements.get(index.parse().ok()?)?;
            let name_len = tag
                .bytes()
                .skip(1)
                .position(|b| b.is_ascii_whitespace() || b == b'/' || b == b'>')?
                + 1;
            let close_len = tag.get(name_len..)?.find('>')? + 1;
            let (open, after) = tag.split_at(name_len);
            let (close, after) = after.split_at(close_len);
            Some((
                Piece::StartTag {
                    element,
                    open,
                    close,
                },
                after,
            ))
        }
    }

    /// `s` with its character references decoded, as [`select`] decodes attribute values.
    ///
    /// # Errors
    ///
    /// Fails if the decoded text needs more room than the plugin's memory has.
    pub fn unescape(s: &str) -> Result<String, String> {
        // `&nGt;`, five bytes, decodes to six.
        wit::unescape(s, room(s.len() + s.len() / 5))
    }

    /// The content of the first element of `html` that `selector`, a CSS selector such as
    /// `"article"`, matches, as HTML: what is between its start tag and its end tag, or
    /// `None` if nothing matches. An element whose end tag is missing has the rest of the
    /// HTML as its content.
    ///
    /// ```ignore
    /// let html = "<nav>Menu</nav><article><p>Text</p></article>";
    /// assert_eq!(html::inner(html, "article")?.as_deref(), Some("<p>Text</p>"));
    /// ```
    ///
    /// # Errors
    ///
    /// Fails if the selector is invalid, the HTML needs more memory than the server allows
    /// to parse, or the content needs more room than the plugin's memory has.
    pub fn inner(html: &str, selector: &str) -> Result<Option<String>, String> {
        wit::inner(html, selector, room(html.len()))
    }

    /// `s` with `&`, `<`, `>`, `"` and `'` escaped, for use as text or as a quoted
    /// attribute value in HTML.
    pub fn escape(s: &str) -> String {
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
}

/// Write `message` to the server log at `level`.
pub fn log(level: Level, message: &str) {
    host::log(level, message);
}

/// Parse a plugin's config, the JSON object [`Plugin::new`] is given.
///
/// # Errors
///
/// Fails, with a message fit for [`Plugin::new`] to return, if the config doesn't
/// deserialize as `T`.
#[cfg(feature = "serde")]
pub fn parse_config<T: serde::de::DeserializeOwned>(config: &str) -> Result<T, String> {
    serde_json::from_str(config).map_err(|e| format!("invalid config: {e}"))
}

/// A Kiki plugin: how to make it from its config.
///
/// One value of the type is made, with [`Plugin::new`], when the plugin loads, and every
/// handler is called on it. The handlers are the methods marked `#[on(<event>)]` in the
/// `impl` block of the type marked [`#[plugin]`](plugin).
///
/// A handler that panics traps: Kiki treats it as having failed (an entry passes through
/// it unchanged), then starts the plugin afresh, calling [`Plugin::new`] again.
pub trait Plugin: Sized + 'static {
    /// Make the plugin, from its config: a JSON object, its manifest's `[config]` table
    /// with its overrides applied.
    ///
    /// # Errors
    ///
    /// An error fails the load, and the plugins that were running keep running.
    fn new(config: &str) -> Result<Self, String>;

    /// Whether this instance of the plugin, made from its config, wants `event`, one of
    /// the events it has a handler for: by default, every one. Return `false` for events
    /// the config has no use for, since Kiki calls into a plugin for every event it
    /// handles. Timers and scans the plugin started are delivered regardless.
    fn wants(&self, event: EventKind) -> bool {
        let _ = event;
        true
    }
}

thread_local! {
    static INSTANCE: RefCell<Option<Box<dyn Any>>> = const { RefCell::new(None) };
}

/// Make the plugin `P` from `config`, for its `init` export, and return which of
/// `handled`, the events it has handlers for, it wants.
#[doc(hidden)]
pub fn __init<P: Plugin>(config: String, handled: &[EventKind]) -> Result<Vec<EventKind>, String> {
    let plugin = P::new(&config)?;
    let events = handled
        .iter()
        .copied()
        .filter(|&e| plugin.wants(e))
        .collect();
    INSTANCE.with(|i| *i.borrow_mut() = Some(Box::new(plugin)));
    Ok(events)
}

/// Call `f` with the plugin `P`, for its other exports.
#[doc(hidden)]
pub fn __with<P: Plugin, R>(f: impl FnOnce(&mut P) -> R) -> R {
    INSTANCE.with(|i| {
        let mut instance = i.borrow_mut();
        let plugin = instance
            .as_mut()
            .and_then(|p| p.downcast_mut::<P>())
            .expect("kiki-plugin: a handler was called before init");
        f(plugin)
    })
}
