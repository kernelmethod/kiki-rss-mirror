//! Sanitizing entry content from feeds for display in the web UI.
//!
//! Entry content is HTML written by whoever runs the feed, so the web UI
//! cannot show it as-is. [`sanitize_html`] keeps a small set of formatting
//! elements, drops every attribute except the links and image sources it
//! can check, and removes everything else.

use crate::tasks::assets::resolve_http_url;
use lol_html::errors::RewritingError;
use lol_html::html_content::{ContentType, Element};
use lol_html::{doc_comments, element, HtmlRewriter, Settings};
use quick_xml::escape::escape;
use url::Url;

/// Elements that are kept, with their attributes stripped. Every other
/// element is unwrapped, leaving its content in place, unless it is one of
/// [`DROPPED`].
const ALLOWED: &[&str] = &[
    "a",
    "abbr",
    "b",
    "blockquote",
    "br",
    "caption",
    "cite",
    "code",
    "dd",
    "del",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "pre",
    "q",
    "s",
    "small",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
];

/// Elements that are removed together with their content. These hold
/// script or styles, embed other documents, or hold raw text that would be
/// parsed as markup if it were unwrapped.
const DROPPED: &[&str] = &[
    "applet",
    "audio",
    "button",
    "embed",
    "frame",
    "frameset",
    "head",
    "iframe",
    "input",
    "math",
    "noembed",
    "noframes",
    "noscript",
    "object",
    "plaintext",
    "script",
    "select",
    "style",
    "svg",
    "template",
    "textarea",
    "title",
    "video",
    "xmp",
];

/// Sanitize the HTML `html` so that it is safe to embed in a web UI page.
///
/// Elements in an allowlist of formatting elements are kept, but lose all
/// of their attributes except:
///
/// - `href` on `<a>`, where the URL is `http(s)` after resolving it
///   against `base`. A link without such a URL loses its `href`.
/// - `src` and `alt` on `<img>`. Images are never loaded from where the
///   feed says they are: `image_src` maps an image's resolved `http(s)` URL
///   to the URL of a local copy. An image with no local copy is replaced
///   by a link to it, and one without an `http(s)` URL is removed.
///
/// Elements that carry script, styles or embedded documents are removed
/// along with their content, as are comments. Any other element is
/// unwrapped, keeping its content.
///
/// # Errors
///
/// Returns an error if `html` could not be rewritten; the caller should
/// then show none of it.
///
/// # Examples
///
/// ```ignore
/// let html = sanitize_html(r#"<p onclick="x()">Hi<script>x()</script></p>"#, None, |_| None)?;
/// assert_eq!(html, "<p>Hi</p>");
/// ```
pub fn sanitize_html(
    html: &str,
    base: Option<&Url>,
    image_src: impl Fn(&Url) -> Option<String>,
) -> Result<String, RewritingError> {
    let resolve = |raw: &str| match base {
        Some(base) => resolve_http_url(raw, base),
        None => Url::parse(raw.trim())
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https")),
    };

    let sanitize_element = |el: &mut Element| {
        let name = el.tag_name().to_ascii_lowercase();
        if DROPPED.contains(&name.as_str()) {
            el.remove();
            return Ok(());
        }
        if !ALLOWED.contains(&name.as_str()) {
            el.remove_and_keep_content();
            return Ok(());
        }

        let href = el.get_attribute("href");
        let src = el.get_attribute("src");
        let alt = el.get_attribute("alt");
        let names: Vec<String> = el.attributes().iter().map(|a| a.name()).collect();
        for attr in names {
            el.remove_attribute(&attr);
        }

        match name.as_str() {
            "a" => {
                if let Some(url) = href.as_deref().and_then(resolve) {
                    el.set_attribute("href", url.as_str())?;
                    el.set_attribute("rel", "noopener noreferrer nofollow")?;
                }
            }
            "img" => match src.as_deref().and_then(resolve) {
                Some(url) => match image_src(&url) {
                    Some(local) => {
                        el.set_attribute("src", &local)?;
                        el.set_attribute("alt", alt.as_deref().unwrap_or(""))?;
                        el.set_attribute("loading", "lazy")?;
                    }
                    None => {
                        let label = match alt.as_deref().map(str::trim) {
                            Some(alt) if !alt.is_empty() => format!("[Image: {alt}]"),
                            _ => "[Image]".to_owned(),
                        };
                        el.replace(
                            &format!(
                                "<a href=\"{}\" rel=\"noopener noreferrer nofollow\">{}</a>",
                                escape(url.as_str()),
                                escape(&label)
                            ),
                            ContentType::Html,
                        );
                    }
                },
                None => el.remove(),
            },
            _ => {}
        }
        Ok(())
    };

    let mut out = Vec::new();
    let mut rewriter = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![element!("*", sanitize_element)],
            document_content_handlers: vec![doc_comments!(|c| {
                c.remove();
                Ok(())
            })],
            ..Settings::new()
        },
        |chunk: &[u8]| out.extend_from_slice(chunk),
    );
    rewriter.write(html.as_bytes())?;
    rewriter.end()?;

    Ok(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sanitize(html: &str) -> String {
        let base = Url::parse("https://example.com/posts/1").unwrap();
        sanitize_html(html, Some(&base), |url| {
            (url.as_str() == "https://example.com/posts/pic.png").then(|| "/assets/pic".to_owned())
        })
        .unwrap()
    }

    #[test]
    fn formatting_is_kept() {
        let html = "<p>Some <em>text</em> and <strong>more</strong>.</p><ul><li>One</li></ul>";
        assert_eq!(sanitize(html), html);
    }

    #[test]
    fn scripts_and_styles_are_removed_with_their_content() {
        assert_eq!(
            sanitize("<p>a<script>alert(1)</script>b<style>p{}</style>c</p>"),
            "<p>abc</p>"
        );
        assert_eq!(sanitize("<svg><script>alert(1)</script></svg>x"), "x");
        assert_eq!(sanitize("<iframe src=\"https://evil\"></iframe>x"), "x");
    }

    /// Unwrapping an element whose content is raw text would turn that
    /// text into markup, so such elements are removed outright.
    #[test]
    fn raw_text_elements_are_not_unwrapped() {
        assert_eq!(sanitize("<xmp><script>alert(1)</script></xmp>x"), "x");
        assert_eq!(
            sanitize("<noscript><script>alert(1)</script></noscript>x"),
            "x"
        );
        assert_eq!(
            sanitize("<textarea><script>alert(1)</script></textarea>x"),
            "x"
        );
    }

    #[test]
    fn unknown_elements_are_unwrapped() {
        assert_eq!(sanitize("<div class=\"x\"><span>kept</span></div>"), "kept");
        assert_eq!(sanitize("<form action=\"/x\">text</form>"), "text");
    }

    #[test]
    fn attributes_are_stripped() {
        assert_eq!(
            sanitize("<p style=\"color:red\" onclick=\"alert(1)\">x</p>"),
            "<p>x</p>"
        );
    }

    #[test]
    fn links_are_resolved_and_checked() {
        assert_eq!(
            sanitize("<a href=\"/about\" onclick=\"x()\">a</a>"),
            "<a href=\"https://example.com/about\" rel=\"noopener noreferrer nofollow\">a</a>"
        );
        assert_eq!(
            sanitize("<a href=\"javascript:alert(1)\">a</a>"),
            "<a>a</a>"
        );
    }

    /// Cached images are shown from their local copy.
    #[test]
    fn images_are_served_from_the_cache() {
        assert_eq!(
            sanitize("<img src=\"pic.png\" alt=\"A pic\" onerror=\"x()\">"),
            "<img src=\"/assets/pic\" alt=\"A pic\" loading=\"lazy\">"
        );
    }

    /// Images that aren't cached are never loaded from the publisher; they
    /// become links instead.
    #[test]
    fn uncached_images_become_links() {
        assert_eq!(
            sanitize("<img src=\"other.png\" alt=\"Other <pic>\">"),
            "<a href=\"https://example.com/posts/other.png\" rel=\"noopener noreferrer nofollow\">[Image: Other &lt;pic&gt;]</a>"
        );
        assert_eq!(
            sanitize("<img src=\"/other.png\">"),
            "<a href=\"https://example.com/other.png\" rel=\"noopener noreferrer nofollow\">[Image]</a>"
        );
    }

    #[test]
    fn images_need_an_http_source() {
        assert_eq!(sanitize("<img src=\"data:image/png;base64,AA\">x"), "x");
    }

    /// A value can't end its attribute early: its quotes are escaped. (`<`
    /// needs no escaping inside a quoted attribute.)
    #[test]
    fn attribute_values_are_escaped() {
        assert_eq!(
            sanitize("<img src=\"pic.png\" alt='\"><script>x()</script>'>"),
            "<img src=\"/assets/pic\" alt=\"&quot;><script>x()</script>\" loading=\"lazy\">"
        );
    }

    #[test]
    fn comments_are_removed() {
        assert_eq!(sanitize("a<!-- hidden -->b"), "ab");
    }

    #[test]
    fn relative_urls_without_a_base_are_dropped() {
        assert_eq!(
            sanitize_html("<a href=\"/about\">a</a>", None, |_| None).unwrap(),
            "<a>a</a>"
        );
    }
}
