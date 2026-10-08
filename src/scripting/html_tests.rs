//! Tests of the `html` interface's two passes.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use super::*;

fn never() -> bool {
    false
}

const MAX: u32 = HTML_OUTPUT_LIMIT_BYTES as u32;

/// The elements `selector` matches in `html`, as tag names and their attributes.
fn elements(html: &str, selector: &str) -> Vec<(String, Vec<(String, String)>)> {
    let selected = select(html, selector, MAX, &never).unwrap();
    selected
        .elements
        .iter()
        .map(|el| {
            let start = el.attributes.start as usize;
            let attributes = selected.attributes[start..start + el.attributes.len as usize]
                .iter()
                .map(|a| {
                    (
                        selected.text(a.name).to_string(),
                        selected.text(a.value).to_string(),
                    )
                })
                .collect();
            (selected.text(el.tag_name).to_string(), attributes)
        })
        .collect()
}

fn edit(element: u32, op: EditOp) -> Edit {
    Edit { element, op }
}

fn rewrite_with(html: &str, selector: &str, edits: Vec<Edit>) -> Result<String, HtmlError> {
    rewrite(html, selector, edits, false, MAX, &never)
}

#[test]
fn elements_are_selected_in_document_order() {
    let found = elements(
        r#"<div><A HREF="/a?x=1&amp;y=2" Title="t">x</A><p>y<a>z</a></p></div>"#,
        "a",
    );
    assert_eq!(
        found,
        [
            (
                "a".to_string(),
                vec![
                    ("href".to_string(), "/a?x=1&y=2".to_string()),
                    ("title".to_string(), "t".to_string())
                ]
            ),
            ("a".to_string(), vec![]),
        ]
    );
}

#[test]
fn namespaces_are_reported() {
    let selected = select("<p></p><svg><a></a></svg><math></math>", "*", MAX, &never).unwrap();
    let namespaces: Vec<_> = selected.elements.iter().map(|e| e.namespace).collect();
    assert_eq!(
        namespaces,
        [
            Namespace::Html,
            Namespace::Svg,
            Namespace::Svg,
            Namespace::MathMl
        ]
    );
}

#[test]
fn without_edits_html_is_unchanged() {
    let html = "<p class=x>Some &amp; <b>text</b><!-- c --></p>";
    assert_eq!(rewrite_with(html, "*", vec![]).unwrap(), html);
}

#[test]
fn comments_can_be_removed() {
    let out = rewrite("a<!-- hidden -->b", "*", vec![], true, MAX, &never).unwrap();
    assert_eq!(out, "ab");
}

#[test]
fn edits_apply_by_index_in_order() {
    let html = r#"<p>a <script>x()</script><a href="/x" onclick="y()">b</a></p>"#;
    // Out of order, as long as each element's own edits are in order.
    let out = rewrite_with(
        html,
        "script, a",
        vec![
            edit(1, EditOp::RemoveAttribute("onclick".into())),
            edit(0, EditOp::Remove),
            edit(1, EditOp::SetAttribute("rel".into(), "x".into())),
            edit(1, EditOp::SetAttribute("rel".into(), "nofollow".into())),
        ],
    )
    .unwrap();
    assert_eq!(out, r#"<p>a <a href="/x" rel="nofollow">b</a></p>"#);
}

/// The descendants of a removed element are still matched, so that the second pass sees
/// the same elements as the first.
#[test]
fn removed_elements_children_keep_their_indices() {
    let html = "<object><param><b>x</b></object><i>y</i>";
    let tags: Vec<_> = elements(html, "*").into_iter().map(|(t, _)| t).collect();
    assert_eq!(tags, ["object", "param", "b", "i"]);
    let out = rewrite_with(
        html,
        "*",
        vec![
            edit(0, EditOp::Remove),
            edit(3, EditOp::SetTagName("em".into())),
        ],
    )
    .unwrap();
    assert_eq!(out, "<em>y</em>");
}

#[test]
fn attribute_values_round_trip() {
    // A value read and set again reads back the same.
    let html = r#"<a href="/a?x=1&amp;y=&quot;2&quot;" title='it&#39;s'>x</a>"#;
    let attributes = &elements(html, "a")[0].1;
    let mut edits = Vec::new();
    for (name, value) in attributes {
        edits.push(edit(0, EditOp::RemoveAttribute(name.clone())));
        edits.push(edit(0, EditOp::SetAttribute(name.clone(), value.clone())));
    }
    assert_eq!(
        rewrite_with(html, "a", edits).unwrap(),
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
        assert_eq!(decode(raw), decoded, "{raw:?}");
    }
}

#[test]
fn content_can_be_inserted_as_text_or_html() {
    let out = rewrite_with(
        "<p>x</p>",
        "p",
        vec![
            edit(0, EditOp::Insert(Place::Before, "<b>".into(), true)),
            edit(0, EditOp::Insert(Place::After, "</b>".into(), false)),
            edit(0, EditOp::Insert(Place::Prepend, "[".into(), false)),
            edit(0, EditOp::Insert(Place::Append, "]".into(), false)),
        ],
    )
    .unwrap();
    assert_eq!(out, "<b><p>[x]</p>&lt;/b&gt;");

    let out = rewrite_with(
        "<div><p>x</p></div><span>y</span>",
        "div, span",
        vec![
            edit(0, EditOp::Insert(Place::Inner, "<i>z</i>".into(), true)),
            edit(1, EditOp::Insert(Place::Replace, "a & b".into(), false)),
        ],
    )
    .unwrap();
    assert_eq!(out, "<div><i>z</i></div>a &amp; b");
}

#[test]
fn elements_can_be_unwrapped() {
    let out = rewrite_with(
        r#"<div class="x"><span>kept</span></div>"#,
        "*",
        vec![edit(0, EditOp::Unwrap), edit(1, EditOp::Unwrap)],
    )
    .unwrap();
    assert_eq!(out, "kept");
}

/// The message `result` failed with.
fn failed<T: std::fmt::Debug>(result: Result<T, HtmlError>) -> String {
    match result {
        Err(HtmlError::Failed(message)) => message,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn bad_calls_are_errors() {
    assert!(failed(select("<p>", "p[", MAX, &never)).contains("invalid selector"));
    assert!(failed(rewrite_with(
        "<p>x</p>",
        "p",
        vec![edit(0, EditOp::SetAttribute("a b".into(), "x".into()))]
    ))
    .contains("invalid attribute name"));
    assert!(failed(rewrite_with(
        "<p>x</p>",
        "p",
        vec![edit(0, EditOp::SetTagName("1".into()))]
    ))
    .contains("invalid tag name"));
    assert_eq!(
        failed(rewrite_with("<p>x</p>", "p", vec![edit(1, EditOp::Remove)])),
        "an edit is for element 1, but the selector matched only 1"
    );
}

#[test]
fn results_are_capped() {
    let html = r#"<p title="abcdefgh">x</p>"#.repeat(64);
    // Each element takes 1 + 5 + 8 bytes of text, an element and an attribute.
    let per_element = (14 + ELEMENT_BYTES + ATTRIBUTE_BYTES) as u32;
    assert_eq!(
        select(&html, "p", 64 * per_element, &never)
            .unwrap()
            .elements
            .len(),
        64
    );
    assert!(select(&html, "p", 64 * per_element - 1, &never).is_err());

    assert!(rewrite(&html, "p", vec![], false, html.len() as u32, &never).is_ok());
    let err = rewrite(&html, "p", vec![], false, html.len() as u32 - 1, &never).unwrap_err();
    assert!(
        matches!(&err, HtmlError::Failed(m) if m.contains("longer than")),
        "{err:?}"
    );

    assert!(unescape("&amp;", 1).is_ok());
    assert!(unescape("&#x20AC;", 2).is_err());
}

#[test]
fn passes_stop_once_the_budget_runs_out() {
    let html = "<p>x</p>".repeat(1000);
    assert_eq!(
        select(&html, "p", MAX, &|| true).unwrap_err(),
        HtmlError::Expired
    );
    assert_eq!(
        rewrite(&html, "p", vec![], false, MAX, &|| true).unwrap_err(),
        HtmlError::Expired
    );
}
