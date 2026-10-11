//! Sanitize the HTML content of new entries as they arrive, so that what Kiki stores, and
//! hands to API clients, is safe to show: it keeps only the elements and attributes it
//! allows, and drops scripts, styles, embedded content, event handlers and links to
//! unsafe URLs.
//!
//! Kiki's `sanitize` plugin, built as WebAssembly by Kiki's `build.rs` (see
//! `plugins/Cargo.toml`) and installed as `plugins/sanitize/plugin.wasm`. Versions before
//! 2.0.0 were written in Lua, on `kiki.html`; this one takes the same config and rewrites
//! HTML the same way, with the same parser, `lol_html`, which Kiki runs for it through the
//! SDK's `html` module.
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
//! behind character references, what is stored is what was checked. Text is kept as it is,
//! but for NUL characters, which become U+FFFD, as browsers show them.
//!
//! The HTML is parsed once: Kiki removes and unwraps elements by their names, as the
//! config says, and takes the attributes off the elements kept (`html::strip`). Which
//! attributes to keep, and which URLs, the plugin decides as it writes the HTML out again.
//!
//! If the HTML cannot be rewritten (its elements or the rewritten HTML need more memory than
//! the plugin has, say), the entry's content is replaced with its text, escaped, so that
//! unsanitized HTML is never stored. The plugin bounds its own memory use, rather than run
//! out of the memory Kiki gives it: a plugin that runs out traps, and the entry it was given
//! would pass through it unsanitized.
//!
//! Only entries as they are fetched are sanitized: plugins cannot change the content of
//! entries already stored. Plugins that run after this one (those whose directory names
//! sort after "sanitize") see the sanitized content, and can add markup back.

use kiki_plugin::html::{self, Piece, StripRules};
use kiki_plugin::{log, plugin, Entry, Level, Plugin};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

/// Longest content the plugin writes as text.
const OUTPUT_LIMIT_BYTES: usize = html::MAX_RESULT_BYTES;

/// How much text [`as_text`] has Kiki unescape at a time, so that it needs little memory
/// besides the text.
const UNESCAPE_CHUNK_BYTES: usize = 64 * 1024;

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

/// FNV-1a. The sets of names the plugin looks every attribute up in hold a few short
/// names, which the default hasher, SipHash, is slow to hash in WebAssembly. Content only
/// looks names up, and never adds any, so it can't make the sets slow to search.
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
}

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

type Set = HashSet<String, BuildHasherDefault<Fnv>>;
type Map<V> = HashMap<String, V, BuildHasherDefault<Fnv>>;

struct Sanitize {
    /// What to do with each element: keep the config's `elements`, remove
    /// [`ALWAYS_DROPPED`] and the config's `drop` with their content, and SVG and MathML
    /// too, and unwrap the rest.
    rules: StripRules,
    /// The URL schemes allowed, lowercase.
    schemes: Set,
    /// The attributes allowed on every element.
    global_attributes: Set,
    /// The attributes allowed on given elements, by element.
    element_attributes: Map<Set>,
}

/// Reads config key `key` as a list of non-empty strings, lowercased, as a set.
fn string_set(config: &serde_json::Map<String, Value>, key: &str) -> Result<Set, String> {
    let list = match config.get(key) {
        None | Some(Value::Null) => return Ok(Set::default()),
        // An empty Lua table, as configs written for the Lua plugin could hold.
        Some(Value::Object(map)) if map.is_empty() => return Ok(Set::default()),
        Some(Value::Array(list)) => list,
        Some(_) => return Err(format!("sanitize: '{key}' must be a list of strings")),
    };
    let mut set = Set::with_capacity_and_hasher(list.len(), Default::default());
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
    fn from_config(config: &serde_json::Map<String, Value>) -> Result<Self, String> {
        let allowed = string_set(config, "elements")?;
        let mut dropped = string_set(config, "drop")?;
        dropped.extend(ALWAYS_DROPPED.iter().map(|s| s.to_string()));
        let schemes = string_set(config, "url_schemes")?;

        let mut global_attributes = Set::default();
        let mut element_attributes: Map<Set> = Map::default();
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
            rules: StripRules {
                // Dropping wins over keeping.
                keep: allowed.difference(&dropped).cloned().collect(),
                drop: dropped.into_iter().collect(),
                drop_foreign: true,
                remove_comments: true,
            },
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
        // The scheme is what comes before the first `:`, if that is a letter followed by
        // letters, digits, `+`, `.` and `-`. Read without copying the URL, as this runs for
        // every link and image.
        let mut chars = url
            .chars()
            .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
            .skip_while(|c| c.is_ascii_control() || *c == ' ');
        let mut scheme = [0; 32];
        let mut len = 0;
        loop {
            match chars.next() {
                Some(':') if len > 0 => break,
                Some(c) if c.is_ascii_alphabetic() || (len > 0 && is_scheme_char(c)) => {
                    let Some(byte) = scheme.get_mut(len) else {
                        return self.long_url_allowed(url);
                    };
                    *byte = c.to_ascii_lowercase() as u8;
                    len += 1;
                }
                // No scheme: the URL is relative.
                _ => return true,
            }
        }
        let scheme = std::str::from_utf8(scheme.get(..len).unwrap_or_default()).unwrap_or_default();
        self.schemes.contains(scheme)
    }

    /// [`Self::url_allowed`], for a URL that may have a scheme too long to read without
    /// copying it.
    fn long_url_allowed(&self, url: &str) -> bool {
        let cleaned: String = url
            .chars()
            .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
            .skip_while(|c| c.is_ascii_control() || *c == ' ')
            .collect();
        let Some((scheme, _)) = cleaned.split_once(':') else {
            return true;
        };
        let is_scheme = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme.chars().all(is_scheme_char);
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

    /// `content`, sanitized, or why it could not be.
    ///
    /// Kiki parses it once, removing and unwrapping elements as [`Self::rules`] say, and
    /// taking every attribute off the elements kept. The plugin then writes the HTML out
    /// again, with the attributes it allows written back from their decoded values: the
    /// value stored is the value checked.
    fn rewrite(&self, content: &str) -> Result<String, String> {
        let stripped = html::strip(content, &self.rules)?;
        let mut out = Bounded::with_capacity(stripped.len() + stripped.len() / 4);
        // The attributes kept on an element, by name, each where it first was.
        let mut kept: Vec<(&str, &str)> = Vec::new();
        for piece in stripped.pieces() {
            let written = match piece {
                Piece::Html(html) => out.push(html.as_bytes()),
                Piece::StartTag {
                    element,
                    open,
                    close,
                } => {
                    let name = element.tag_name();
                    kept.clear();
                    for (attribute, value) in element.attributes() {
                        if !self.attribute_allowed(name, attribute)
                            || !self.value_allowed(attribute, value)
                        {
                            continue;
                        }
                        // A repeated attribute's last value wins.
                        match kept.iter_mut().find(|(n, _)| *n == attribute) {
                            Some(same) => same.1 = value,
                            None => kept.push((attribute, value)),
                        }
                    }
                    if name == "img" && !kept.iter().any(|(attribute, _)| *attribute == "src") {
                        continue;
                    }
                    write_start_tag(&mut out, open, &kept, close)
                }
            };
            if !written {
                return Err("the sanitized HTML does not fit in the plugin's memory".to_string());
            }
        }
        Ok(out.into_string())
    }
}

/// Write a start tag: `open`, the `<` and the tag name; ` name="value"` for each of
/// `attributes`, with `&` and `"` in the value escaped; and `close`, the rest of the tag,
/// after a space if it is `/>` and follows an attribute, whose value it would otherwise
/// end if unquoted. Returns whether it all fit.
fn write_start_tag(
    out: &mut Bounded,
    open: &str,
    attributes: &[(&str, &str)],
    close: &str,
) -> bool {
    let mut fits = out.push(open.as_bytes());
    for &(name, value) in attributes {
        // Written only with a name that can't end the tag or start a value.
        if !is_attribute_name(name) {
            continue;
        }
        fits &= out.push(b" ") && out.push(name.as_bytes()) && out.push(b"=\"");
        let mut rest = value;
        while let Some(i) = rest.find(['&', '"']) {
            let (text, after) = rest.split_at(i);
            let escaped: &[u8] = if after.starts_with('&') {
                b"&amp;"
            } else {
                b"&quot;"
            };
            fits &= out.push(text.as_bytes()) && out.push(escaped);
            rest = after.get(1..).unwrap_or_default();
        }
        fits &= out.push(rest.as_bytes()) && out.push(b"\"");
    }
    if !attributes.is_empty() && close.starts_with('/') {
        fits &= out.push(b" ");
    }
    fits && out.push(close.as_bytes())
}

/// Whether `name` can be written as an attribute's name: it is not empty, and has no
/// whitespace, control characters, quotes, `/`, `=` or `>`.
fn is_attribute_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '/' | '=' | '>' | '<')
        })
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

/// Whether `c` may follow a URL scheme's first letter.
fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')
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
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let (piece, after) = rest.split_at(unescape_split(rest));
        rest = after;
        // Out of room to unescape the next piece: what is written so far will do.
        let Ok(piece) = html::unescape(piece) else {
            break;
        };
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
                return out.into_string();
            }
        }
    }
    out.into_string()
}

/// Where to cut the first piece of `text` to unescape: after about
/// [`UNESCAPE_CHUNK_BYTES`], before a character that cannot be part of a character
/// reference, which is an `&` followed by ASCII letters, digits, `#` and `;`. Only a
/// numeric reference longer than the chunk, with thousands of digits, is cut in two, and
/// the text is escaped again in any case.
fn unescape_split(text: &str) -> usize {
    if text.len() <= UNESCAPE_CHUNK_BYTES {
        return text.len();
    }
    let mut end = UNESCAPE_CHUNK_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end)
        .unwrap_or_default()
        .char_indices()
        .rev()
        .find(|&(i, c)| i > 0 && !(c.is_ascii_alphanumeric() || matches!(c, '#' | ';')))
        .map_or(end, |(i, _)| i)
}

impl Plugin for Sanitize {
    fn new(config: &str) -> Result<Self, String> {
        let config: serde_json::Map<String, Value> =
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
