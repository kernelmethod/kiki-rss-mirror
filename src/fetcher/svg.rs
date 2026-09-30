//! Sanitizing SVG images downloaded from feeds.
//!
//! An SVG is an XML document that can carry script, event handlers, links,
//! and references to other resources, and a browser runs or follows all of
//! them when the SVG is opened on its own. [`sanitize_svg`] therefore
//! rebuilds the document from an allowlist: only known drawing elements
//! and presentation attributes are written out, and everything else —
//! scripts, `foreignObject`, `image`, `use`, links, filters, event
//! handlers, the DOCTYPE, comments — is dropped. The output is what is
//! cached and served, never the original bytes.
//!
//! It is a function of its input alone, so it runs in the sandboxed feed
//! fetcher, and again on the server, which does not trust what the fetcher
//! returns. Sanitizing is idempotent.

use quick_xml::escape::escape;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// The media type of SVG images.
pub const SVG_CONTENT_TYPE: &str = "image/svg+xml";

/// The largest SVG that will be sanitized. Icons and illustrations in
/// feeds are far smaller; bigger documents are more likely to be an attempt
/// to exhaust a browser than an image.
pub const MAX_SVG_BYTES: usize = 1024 * 1024;

/// The deepest element nesting accepted.
const MAX_DEPTH: usize = 64;

/// Elements that are kept. Everything else is removed along with its
/// content. `use`, `symbol` and `pattern` are left out on purpose: they let
/// a small document expand into an enormous one.
const ALLOWED_ELEMENTS: &[&str] = &[
    "svg",
    "g",
    "defs",
    "path",
    "circle",
    "ellipse",
    "line",
    "polyline",
    "polygon",
    "rect",
    "text",
    "tspan",
    "title",
    "desc",
    "style",
    "linearGradient",
    "radialGradient",
    "stop",
    "clipPath",
    "mask",
    "marker",
];

/// Elements whose text content is kept.
const TEXT_ELEMENTS: &[&str] = &["text", "tspan", "title", "desc", "style"];

/// Attributes that are kept, on any allowed element. `href` and
/// `xlink:href` are handled separately.
const ALLOWED_ATTRIBUTES: &[&str] = &[
    "id",
    "class",
    "style",
    "transform",
    "viewBox",
    "preserveAspectRatio",
    "width",
    "height",
    "x",
    "y",
    "x1",
    "y1",
    "x2",
    "y2",
    "cx",
    "cy",
    "r",
    "rx",
    "ry",
    "fx",
    "fy",
    "fr",
    "d",
    "points",
    "pathLength",
    "fill",
    "fill-opacity",
    "fill-rule",
    "stroke",
    "stroke-width",
    "stroke-opacity",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-dasharray",
    "stroke-dashoffset",
    "opacity",
    "color",
    "display",
    "visibility",
    "clip-path",
    "clip-rule",
    "mask",
    "offset",
    "stop-color",
    "stop-opacity",
    "gradientUnits",
    "gradientTransform",
    "spreadMethod",
    "clipPathUnits",
    "maskUnits",
    "maskContentUnits",
    "markerWidth",
    "markerHeight",
    "markerUnits",
    "refX",
    "refY",
    "orient",
    "font-family",
    "font-size",
    "font-weight",
    "font-style",
    "text-anchor",
    "dominant-baseline",
    "dx",
    "dy",
    "xml:space",
];

/// Elements that may link to another element in the document, to share
/// its gradient stops.
const HREF_ELEMENTS: &[&str] = &["linearGradient", "radialGradient"];

/// CSS functions that can load a resource from a URL given as a string.
const FETCHING_CSS_FUNCTIONS: &[&str] = &["image-set(", "image(", "src(", "element(", "paint("];

/// Other text that has no place in an icon's styles.
const FORBIDDEN_CSS: &[&str] = &["@", "\\", "/*", "expression", "behavior", "binding"];

/// Whether `value`, from an attribute or a `<style>` element, is free of
/// anything that could load a resource or run code: only `url(#fragment)`
/// references to the document itself are allowed.
fn is_safe_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("javascript:")
        || lower.contains("data:")
        || FORBIDDEN_CSS.iter().any(|s| lower.contains(s))
        || FETCHING_CSS_FUNCTIONS.iter().any(|s| lower.contains(s))
    {
        return false;
    }
    lower.match_indices("url(").all(|(at, found)| {
        lower[at + found.len()..]
            .trim_start_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .starts_with('#')
    })
}

/// Whether `value` is a reference to an element of the same document.
fn is_fragment(value: &str) -> bool {
    let value = value.trim();
    value.len() > 1 && value.starts_with('#') && is_safe_value(value)
}

/// Write the start tag for `element`, keeping only its allowed attributes.
/// Returns `None` if the attributes cannot be parsed.
fn write_start(out: &mut String, name: &str, element: &BytesStart, empty: bool) -> Option<()> {
    out.push('<');
    out.push_str(name);
    if name == "svg" {
        out.push_str(
            " xmlns=\"http://www.w3.org/2000/svg\" \
             xmlns:xlink=\"http://www.w3.org/1999/xlink\"",
        );
    }
    for attr in element.attributes() {
        let attr = attr.ok()?;
        let key = std::str::from_utf8(attr.key.as_ref()).ok()?;
        let value = attr.unescape_value().ok()?;
        let allowed = if matches!(key, "href" | "xlink:href") {
            HREF_ELEMENTS.contains(&name) && is_fragment(&value)
        } else {
            ALLOWED_ATTRIBUTES.contains(&key) && is_safe_value(&value)
        };
        if allowed {
            out.push(' ');
            out.push_str(key);
            out.push_str("=\"");
            out.push_str(&escape(value.as_ref()));
            out.push('"');
        }
    }
    out.push_str(if empty { "/>" } else { ">" });
    Some(())
}

/// Rebuild the SVG document `input` from the elements and attributes that
/// are known to be safe, as described in the [module documentation](self).
///
/// Returns `None` if `input` is not a well-formed UTF-8 XML document whose
/// root element is `svg`, is nested too deeply, or is over
/// [`MAX_SVG_BYTES`]. Content that is merely not allowed is removed, not
/// grounds for rejecting the whole image.
///
/// # Examples
///
/// ```
/// use kiki_rss::fetcher::svg::sanitize_svg;
///
/// let clean = sanitize_svg(
///     br##"<svg viewBox="0 0 8 8" onload="alert(1)">
///     <script>alert(2)</script><path d="M0 0h8v8z" fill="red"/></svg>"##,
/// )
/// .unwrap();
/// let clean = String::from_utf8(clean).unwrap();
/// assert!(!clean.contains("script") && !clean.contains("onload"));
/// assert!(clean.contains(r#"<path d="M0 0h8v8z" fill="red"/>"#));
///
/// assert!(sanitize_svg(b"<html></html>").is_none());
/// ```
pub fn sanitize_svg(input: &[u8]) -> Option<Vec<u8>> {
    if input.len() > MAX_SVG_BYTES {
        return None;
    }
    let text = std::str::from_utf8(input).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let mut reader = Reader::from_str(text);
    let mut out = String::with_capacity(text.len());
    // Kept elements that are currently open.
    let mut open: Vec<String> = Vec::new();
    // How deep inside a removed element the reader is.
    let mut removed = 0usize;
    let mut started = false;
    let mut finished = false;

    // The name of a start or empty tag if the element is to be kept.
    let kept_name = |element: &BytesStart| -> Option<String> {
        let name = std::str::from_utf8(element.name().as_ref())
            .ok()?
            .to_owned();
        ALLOWED_ELEMENTS.contains(&name.as_str()).then_some(name)
    };

    loop {
        match reader.read_event().ok()? {
            Event::Start(element) => {
                if removed > 0 {
                    removed += 1;
                    continue;
                }
                if finished || (!started && kept_name(&element).as_deref() != Some("svg")) {
                    return None;
                }
                started = true;
                match kept_name(&element) {
                    Some(name) if open.len() < MAX_DEPTH => {
                        write_start(&mut out, &name, &element, false)?;
                        open.push(name);
                    }
                    Some(_) => return None,
                    None => removed = 1,
                }
            }
            Event::Empty(element) => {
                if removed > 0 {
                    continue;
                }
                if finished || (!started && kept_name(&element).as_deref() != Some("svg")) {
                    return None;
                }
                started = true;
                if let Some(name) = kept_name(&element) {
                    write_start(&mut out, &name, &element, true)?;
                    finished |= open.is_empty();
                }
            }
            Event::End(_) => {
                if removed > 0 {
                    removed -= 1;
                    continue;
                }
                let name = open.pop()?;
                out.push_str("</");
                out.push_str(&name);
                out.push('>');
                finished |= open.is_empty();
            }
            Event::Text(_) | Event::CData(_) if removed > 0 => {}
            Event::Text(text) => {
                let Some(parent) = open.last() else { continue };
                if TEXT_ELEMENTS.contains(&parent.as_str()) {
                    let text = text.unescape().ok()?;
                    if parent != "style" || is_safe_value(&text) {
                        out.push_str(&escape(text.as_ref()));
                    }
                }
            }
            Event::CData(data) => {
                let Some(parent) = open.last() else { continue };
                if TEXT_ELEMENTS.contains(&parent.as_str()) {
                    let text = std::str::from_utf8(&data).ok()?;
                    if parent != "style" || is_safe_value(text) {
                        out.push_str(&escape(text));
                    }
                }
            }
            // The XML declaration, DOCTYPE (and any entities it declares),
            // processing instructions and comments carry nothing to draw.
            Event::Decl(_) | Event::DocType(_) | Event::PI(_) | Event::Comment(_) => {}
            Event::Eof => break,
        }
    }

    (finished && open.is_empty() && removed == 0).then(|| out.into_bytes())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn clean(input: &str) -> String {
        String::from_utf8(sanitize_svg(input.as_bytes()).unwrap()).unwrap()
    }

    const HEAD: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" \
        xmlns:xlink=\"http://www.w3.org/1999/xlink\"";

    #[test]
    fn keeps_a_plain_drawing() {
        let input = r##"<?xml version="1.0"?><!-- made by hand -->
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" width="16" height="16">
            <defs><linearGradient id="g"><stop offset="0" stop-color="#fff"/></linearGradient></defs>
            <g transform="scale(2)"><circle cx="4" cy="4" r="3" fill="url(#g)"/></g></svg>"##;
        assert_eq!(
            clean(input),
            format!(
                "{HEAD} viewBox=\"0 0 16 16\" width=\"16\" height=\"16\">\
                 <defs><linearGradient id=\"g\"><stop offset=\"0\" stop-color=\"#fff\"/>\
                 </linearGradient></defs>\
                 <g transform=\"scale(2)\"><circle cx=\"4\" cy=\"4\" r=\"3\" fill=\"url(#g)\"/></g></svg>"
            )
        );
    }

    #[test]
    fn removes_script_and_event_handlers() {
        let out = clean(
            r#"<svg onload="alert(1)"><script>alert(2)</script>
            <rect width="1" height="1" onclick="alert(3)" onmouseover="x()"/>
            <g><script href="//evil/x.js"/></g></svg>"#,
        );
        assert_eq!(
            out,
            format!("{HEAD}><rect width=\"1\" height=\"1\"/><g></g></svg>")
        );
    }

    #[test]
    fn removes_elements_that_embed_or_link_with_their_content() {
        for element in [
            "<foreignObject><iframe src=\"//evil\"/></foreignObject>",
            "<image href=\"https://evil/x.png\"/>",
            "<a href=\"javascript:alert(1)\"><rect width=\"1\" height=\"1\"/></a>",
            "<use href=\"#x\"/>",
            "<animate attributeName=\"href\" to=\"javascript:alert(1)\"/>",
            "<set attributeName=\"onload\" to=\"alert(1)\"/>",
            "<filter id=\"f\"><feImage href=\"//evil\"/></filter>",
        ] {
            assert_eq!(
                clean(&format!("<svg>{element}</svg>")),
                format!("{HEAD}></svg>"),
                "{element}"
            );
        }
    }

    #[test]
    fn only_links_to_the_document_itself() {
        let out = clean(
            r##"<svg><linearGradient id="a" href="//evil/x"/><linearGradient id="b" xlink:href="#a"/>
            <linearGradient id="c" href="javascript:alert(1)"/><rect href="#a"/></svg>"##,
        );
        assert!(out.contains(r##"<linearGradient id="b" xlink:href="#a"/>"##));
        assert!(!out.contains("evil") && !out.contains("javascript"));
        assert!(out.contains("<rect/>"));
    }

    #[test]
    fn drops_values_that_load_resources() {
        let out = clean(
            r#"<svg><rect fill="url(https://evil/x)" style="background:url('//evil')"/>
            <rect style="fill:url(#g);stroke:red" stroke="javascript:alert(1)"/>
            <rect style="@import 'x'" fill="url( &quot;data:image/png;base64,AA&quot; )"/></svg>"#,
        );
        assert!(!out.contains("evil") && !out.contains("javascript") && !out.contains("@"));
        assert!(!out.contains("data:"));
        assert!(out.contains(r##"style="fill:url(#g);stroke:red""##));
    }

    #[test]
    fn keeps_safe_styles_and_drops_unsafe_ones() {
        let out = clean(
            "<svg><style><![CDATA[.a{fill:#f00}]]></style><style>@import url(//evil/x.css);</style></svg>",
        );
        assert_eq!(
            out,
            format!("{HEAD}><style>.a{{fill:#f00}}</style><style></style></svg>")
        );
    }

    #[test]
    fn keeps_text_escaped() {
        assert_eq!(
            clean("<svg><text>a &amp; b &lt;script&gt;</text></svg>"),
            format!("{HEAD}><text>a &amp; b &lt;script&gt;</text></svg>")
        );
    }

    #[test]
    fn text_outside_text_elements_is_dropped() {
        assert_eq!(
            clean("<svg>hello<g>world</g></svg>"),
            format!("{HEAD}><g></g></svg>")
        );
    }

    #[test]
    fn rejects_what_is_not_an_svg_document() {
        for input in [
            &b""[..],
            b"not xml",
            b"<html><body/></html>",
            b"<script>alert(1)</script>",
            b"<svg><g></svg>",
            b"<svg></g></svg>",
            b"<svg>",
            b"<svg/><svg/>",
            b"<svg/><script/>",
            b"<svg xmlns:a=\"x\" a=\"1\" a=\"2\"/>",
            b"<svg><text>&unknown;</text></svg>",
            b"\xff\xfe<\0s\0v\0g\0/\0>\0",
        ] {
            assert!(
                sanitize_svg(input).is_none(),
                "{:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn entities_declared_in_the_doctype_are_not_expanded() {
        let input = "<!DOCTYPE svg [<!ENTITY a \"<script>\">]><svg><text>&a;</text></svg>";
        assert!(sanitize_svg(input.as_bytes()).is_none());
        assert_eq!(
            clean("<!DOCTYPE svg [<!ENTITY a \"x\">]><svg><rect/></svg>"),
            format!("{HEAD}><rect/></svg>")
        );
    }

    #[test]
    fn prefixed_elements_are_removed() {
        assert_eq!(
            clean("<svg><x:script xmlns:x=\"http://www.w3.org/2000/svg\">alert(1)</x:script><rect/></svg>"),
            format!("{HEAD}><rect/></svg>")
        );
    }

    #[test]
    fn rejects_documents_that_are_too_big_or_too_deep() {
        let deep = format!("<svg>{}{}</svg>", "<g>".repeat(100), "</g>".repeat(100));
        assert!(sanitize_svg(deep.as_bytes()).is_none());
        let ok = format!("<svg>{}{}</svg>", "<g>".repeat(10), "</g>".repeat(10));
        assert!(sanitize_svg(ok.as_bytes()).is_some());
        let big = format!("<svg><title>{}</title></svg>", "a".repeat(MAX_SVG_BYTES));
        assert!(sanitize_svg(big.as_bytes()).is_none());
    }

    #[test]
    fn a_byte_order_mark_is_accepted() {
        assert!(sanitize_svg("\u{feff}<svg/>".as_bytes()).is_some());
    }

    #[test]
    fn sanitizing_is_idempotent() {
        let once = clean(
            r##"<svg viewBox="0 0 1 1" onload="x()"><style>.a{fill:url(#g)}</style>
            <text x="1">it's "quoted" &amp; &lt;b&gt;</text>
            <linearGradient id="g" xlink:href="#h"/><path d="M0 0" fill='url("#g")'/></svg>"##,
        );
        assert_eq!(clean(&once), once);
    }
}
