//! Sanitize the HTML content of new entries as they arrive, so that what Kiki stores, and
//! hands to API clients, is safe to show: it keeps only the elements and attributes it
//! allows, and drops scripts, styles, embedded content, event handlers and links to
//! unsafe URLs.
//!
//! Kiki's `sanitize` plugin, built as WebAssembly by Kiki's `build.rs` (see
//! `plugins/Cargo.toml`) and installed as `plugins/sanitize/plugin.wasm`. Versions before
//! 2.0.0 were written in Lua, on `kiki.html`; this one takes the same config and rewrites
//! HTML the same way, with the same parser, `lol_html`, built into the plugin.
//!
//! Config:
//!
//! * `elements`: a list of the elements to keep. Any other element is unwrapped: its tags
//!   are removed and its content kept.
//! * `drop`: a list of elements to remove together with their content, rather than
//!   unwrapping them. Whatever this says, the elements in [`ALWAYS_DROPPED`] are always
//!   removed.
//! * `attributes`: a list of the attributes to keep. A bare name, such as `"title"`, keeps
//!   that attribute on every element; one written `"element:name"`, such as `"a:href"`,
//!   keeps it only on that element. Every other attribute is removed, and event handler
//!   attributes (`onclick`, `onerror`, ...) always are.
//! * `url_schemes`: a list of the URL schemes links and images may use, such as
//!   `"https"`. Relative URLs are always allowed. A URL attribute (`href`, `src`, ...)
//!   whose scheme is not listed is removed, and an image left without a source is removed.
//!
//! Comments are removed. The attributes that are kept are written out again from their
//! decoded values, so a value is stored as it was checked: however a URL hides its scheme
//! behind character references, what is stored is what was checked. Text is kept as it is.
//!
//! If the HTML cannot be rewritten (it needs more memory than the plugin allows itself,
//! say), the entry's content is replaced with its text, escaped, so that unsanitized HTML
//! is never stored. The plugin bounds its own memory use, rather than run out of the
//! memory Kiki gives it: a plugin that runs out traps, and the entry it was given would
//! pass through it unsanitized.
//!
//! Only entries as they are fetched are sanitized: plugins cannot change the content of
//! entries already stored. Plugins that run after this one (those whose directory names
//! sort after "sanitize") see the sanitized content, and can add markup back.

use kiki_plugin::{log, plugin, Entry, Level, Plugin};
use lol_html::html_content::{Comment, Element};
use lol_html::{
    DocumentContentHandlers, ElementContentHandlers, HtmlRewriter, MemorySettings, Settings,
};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// Most memory the rewriter may use for its buffers during one rewrite.
const HTML_MEMORY_LIMIT_BYTES: usize = 4 * 1024 * 1024;

/// Longest content the plugin writes, rewritten or as text.
const OUTPUT_LIMIT_BYTES: usize = 8 * 1024 * 1024;

/// Elements removed with their content whatever the config says: they run scripts, apply
/// styles, embed other documents, change how the page around them behaves, or hold raw
/// text that would be parsed as markup if they were unwrapped.
const ALWAYS_DROPPED: &[&str] = &[
    "applet",
    "base",
    "embed",
    "frame",
    "frameset",
    "head",
    "iframe",
    "link",
    "math",
    "meta",
    "noembed",
    "noframes",
    "noscript",
    "object",
    "param",
    "plaintext",
    "script",
    "style",
    "svg",
    "template",
    "textarea",
    "title",
    "xmp",
];

/// Attributes whose values are URLs, which must be relative or use one of the allowed
/// schemes.
const URL_ATTRIBUTES: &[&str] = &[
    "action",
    "background",
    "cite",
    "codebase",
    "data",
    "formaction",
    "href",
    "longdesc",
    "ping",
    "poster",
    "src",
    "usemap",
];

struct Sanitize {
    /// The elements kept.
    allowed: HashSet<String>,
    /// The elements removed with their content: [`ALWAYS_DROPPED`] and the config's `drop`.
    dropped: HashSet<String>,
    /// The URL schemes allowed, lowercase.
    schemes: HashSet<String>,
    /// The attributes allowed on every element.
    global_attributes: HashSet<String>,
    /// The attributes allowed on given elements, by element.
    element_attributes: HashMap<String, HashSet<String>>,
}

/// Reads config key `key` as a list of non-empty strings, lowercased, as a set.
fn string_set(config: &Map<String, Value>, key: &str) -> Result<HashSet<String>, String> {
    let list = match config.get(key) {
        None | Some(Value::Null) => return Ok(HashSet::new()),
        // An empty Lua table, as configs written for the Lua plugin could hold.
        Some(Value::Object(map)) if map.is_empty() => return Ok(HashSet::new()),
        Some(Value::Array(list)) => list,
        Some(_) => return Err(format!("sanitize: '{key}' must be a list of strings")),
    };
    let mut set = HashSet::with_capacity(list.len());
    for (i, value) in list.iter().enumerate() {
        match value.as_str() {
            Some(s) if !s.is_empty() => set.insert(s.to_ascii_lowercase()),
            _ => {
                return Err(format!(
                    "sanitize: '{key}' entry {} must be a non-empty string",
                    i + 1
                ))
            }
        };
    }
    Ok(set)
}

impl Sanitize {
    fn from_config(config: &Map<String, Value>) -> Result<Self, String> {
        let allowed = string_set(config, "elements")?;
        let mut dropped = string_set(config, "drop")?;
        dropped.extend(ALWAYS_DROPPED.iter().map(|s| s.to_string()));
        let schemes = string_set(config, "url_schemes")?;

        let mut global_attributes = HashSet::new();
        let mut element_attributes: HashMap<String, HashSet<String>> = HashMap::new();
        for value in string_set(config, "attributes")? {
            // "element:name", with neither part empty; anything else is a bare name.
            match value.split_once(':') {
                Some((element, name)) if !element.is_empty() && !name.is_empty() => {
                    element_attributes
                        .entry(element.to_string())
                        .or_default()
                        .insert(name.to_string());
                }
                _ => {
                    global_attributes.insert(value);
                }
            }
        }

        Ok(Sanitize {
            allowed,
            dropped,
            schemes,
            global_attributes,
            element_attributes,
        })
    }

    fn attribute_allowed(&self, element: &str, name: &str) -> bool {
        if name.starts_with("on") {
            return false;
        }
        self.global_attributes.contains(name)
            || self
                .element_attributes
                .get(element)
                .is_some_and(|allowed| allowed.contains(name))
    }

    /// Whether `url` is relative or uses an allowed scheme. Browsers ignore control
    /// characters and spaces at the start of a URL, and tabs and newlines anywhere in it,
    /// so they are ignored here too: otherwise `" java\tscript:"` would look relative.
    fn url_allowed(&self, url: &str) -> bool {
        let cleaned: String = url
            .chars()
            .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
            .skip_while(|c| c.is_ascii_control() || *c == ' ')
            .collect();
        let Some((scheme, _)) = cleaned.split_once(':') else {
            return true;
        };
        let is_scheme = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
        !is_scheme || self.schemes.contains(&scheme.to_ascii_lowercase())
    }

    /// Whether every URL in `srcset`, a list of image candidates such as
    /// `"a.png 1x, b.png 2x"`, is allowed.
    fn srcset_allowed(&self, srcset: &str) -> bool {
        srcset
            .split(',')
            .filter_map(|candidate| candidate.split(is_space).find(|url| !url.is_empty()))
            .all(|url| self.url_allowed(url))
    }

    fn value_allowed(&self, name: &str, value: &str) -> bool {
        if name == "srcset" {
            return self.srcset_allowed(value);
        }
        !URL_ATTRIBUTES.contains(&name) || self.url_allowed(value)
    }

    fn sanitize_element(&self, el: &mut Element) {
        let name = el.tag_name();
        let foreign = matches!(
            el.namespace_uri(),
            "http://www.w3.org/2000/svg" | "http://www.w3.org/1998/Math/MathML"
        );
        if foreign || self.dropped.contains(&name) {
            el.remove();
            return;
        }
        if !self.allowed.contains(&name) {
            el.remove_and_keep_content();
            return;
        }

        // Remove every attribute, then set the ones that are allowed again from their
        // decoded values.
        let attributes: Vec<(String, String)> = el
            .attributes()
            .iter()
            .map(|a| (a.name(), a.value()))
            .collect();
        for (attribute, _) in &attributes {
            el.remove_attribute(attribute);
        }
        let mut has_src = false;
        for (attribute, value) in &attributes {
            let value = decode_attribute(value);
            if self.attribute_allowed(&name, attribute)
                && self.value_allowed(attribute, &value)
                && el
                    .set_attribute(attribute, &escape_attribute(&value))
                    .is_ok()
            {
                has_src |= attribute == "src";
            }
        }

        if name == "img" && !has_src {
            el.remove();
        }
    }

    /// `html`, sanitized, or why it could not be.
    fn rewrite(&self, html: &str) -> Result<String, String> {
        // Sanitized HTML is seldom longer than it was.
        let mut out = Bounded::with_capacity(html.len());
        let mut rewriter = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![(
                    Cow::Owned("*".parse().map_err(|e| format!("{e}"))?),
                    ElementContentHandlers::default().element(|el: &mut Element| {
                        self.sanitize_element(el);
                        Ok(())
                    }),
                )],
                document_content_handlers: vec![DocumentContentHandlers::default().comments(
                    |c: &mut Comment| {
                        c.remove();
                        Ok(())
                    },
                )],
                memory_settings: MemorySettings {
                    max_allowed_memory_usage: HTML_MEMORY_LIMIT_BYTES,
                    ..MemorySettings::default()
                },
                ..Settings::new()
            },
            |chunk: &[u8]| {
                out.push(chunk);
            },
        );
        let result = rewriter
            .write(html.as_bytes())
            .and_then(|()| rewriter.end());
        if out.full {
            return Err(format!(
                "the rewritten HTML is longer than {OUTPUT_LIMIT_BYTES} bytes, or too large \
                 to hold"
            ));
        }
        result.map_err(|e| format!("unable to rewrite the HTML: {e}"))?;
        // The output is the input's UTF-8, with ASCII markup removed or added.
        String::from_utf8(out.bytes).map_err(|e| format!("the rewritten HTML is not UTF-8: {e}"))
    }
}

/// Bytes written up to [`OUTPUT_LIMIT_BYTES`], or as many as the plugin's memory holds,
/// whichever is fewer. Running out of memory would trap, and let the entry through
/// unsanitized, so the plugin's large allocations go through here.
#[derive(Default)]
struct Bounded {
    bytes: Vec<u8>,
    /// Whether something was left out.
    full: bool,
}

impl Bounded {
    /// An empty buffer with room for `len` bytes, if the plugin's memory has it. Sizing the
    /// buffer once spares the memory doubling it as it fills would leave unused: the
    /// plugin's memory never shrinks, and holds the entry's content besides.
    fn with_capacity(len: usize) -> Self {
        let mut bytes = Vec::new();
        let _ = bytes.try_reserve_exact(len.min(OUTPUT_LIMIT_BYTES));
        Bounded { bytes, full: false }
    }

    /// Append `piece`, or note that it did not fit and return false.
    fn push(&mut self, piece: &[u8]) -> bool {
        let len = self.bytes.len();
        let fits = !self.full
            && len + piece.len() <= OUTPUT_LIMIT_BYTES
            && (self.bytes.try_reserve(piece.len()).is_ok()
                // Too little memory to double the buffer; grow it by an eighth.
                || self
                    .bytes
                    .try_reserve_exact((len / 8).max(piece.len()))
                    .is_ok());
        if fits {
            self.bytes.extend_from_slice(piece);
        } else {
            self.full = true;
        }
        fits
    }

    /// The text written, which was pushed as whole `str`s.
    fn into_string(self) -> String {
        String::from_utf8(self.bytes).unwrap_or_default()
    }
}

/// Whether `c` is whitespace as the Lua plugin's patterns had it (`%s`): ASCII
/// whitespace, and the vertical tab.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r')
}

/// The text of `html`, escaped: what an entry's content becomes when it cannot be
/// sanitized. Tags are removed, character references decoded, and the text escaped again;
/// text past [`OUTPUT_LIMIT_BYTES`] once escaped, or past what the plugin's memory can
/// hold, is cut off.
fn as_text(html: String) -> String {
    // Tags are removed in place, so that this needs memory only for the escaped text.
    let mut text = html.into_bytes();
    let mut kept = 0;
    let mut read = 0;
    while read < text.len() {
        let rest = text.get(read..).unwrap_or_default();
        // A `<` with no `>` after it is kept, as text.
        let tag = rest.iter().position(|&b| b == b'<').and_then(|start| {
            let len = rest.get(start..)?.iter().position(|&b| b == b'>')?;
            Some((start, len))
        });
        let (end, next) = match tag {
            Some((start, len)) => (read + start, read + start + len + 1),
            None => (text.len(), text.len()),
        };
        text.copy_within(read..end, kept);
        kept += end - read;
        read = next;
    }
    text.truncate(kept);
    // The bytes removed run from an ASCII `<` to an ASCII `>`, so the rest is UTF-8.
    let text = String::from_utf8(text).unwrap_or_default();

    let mut out = Bounded::with_capacity(text.len() + text.len() / 8);
    let mut buf = [0; 4];
    decode(&text, |piece| {
        for c in piece.chars() {
            let escaped = match c {
                '&' => "&amp;",
                '<' => "&lt;",
                '>' => "&gt;",
                '"' => "&quot;",
                '\'' => "&#39;",
                c => c.encode_utf8(&mut buf),
            };
            if !out.push(escaped.as_bytes()) {
                return false;
            }
        }
        true
    });
    out.into_string()
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
/// as they are. The same decoding as `kiki.html`'s, which the Lua plugin used.
fn decode_attribute(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    decode(s, |piece| {
        out.push_str(piece);
        true
    });
    Cow::Owned(out)
}

/// Decode the character references in `s` as [`decode_attribute`] does, passing the
/// decoded text to `push` piece by piece, until it returns false.
fn decode(s: &str, mut push: impl FnMut(&str) -> bool) {
    let mut rest = s;
    while let Some((before, after)) = rest.split_once('&') {
        if !push(before) {
            return;
        }
        let pushed = match decode_reference(after) {
            Some((decoded, len)) => {
                rest = after.get(len..).unwrap_or_default();
                push(&decoded)
            }
            None => {
                rest = after;
                push("&")
            }
        };
        if !pushed {
            return;
        }
    }
    push(rest);
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

impl Plugin for Sanitize {
    fn new(config: &str) -> Result<Self, String> {
        let config: Map<String, Value> =
            serde_json::from_str(config).map_err(|e| format!("sanitize: invalid config: {e}"))?;
        Sanitize::from_config(&config)
    }
}

#[plugin]
impl Sanitize {
    #[on(entry.ingest)]
    fn sanitize_ingested(&mut self, mut entry: Entry) -> Option<Entry> {
        if let Some(content) = entry.content.take() {
            entry.content = Some(match self.rewrite(&content) {
                Ok(sanitized) => sanitized,
                Err(e) => {
                    log(
                        Level::Warn,
                        &format!(
                            "sanitize: unable to sanitize the content of entry {}, keeping \
                             only its text: {e}",
                            entry.guid
                        ),
                    );
                    as_text(content)
                }
            });
        }
        Some(entry)
    }
}
