use super::api::encode_path_segment;
use serde::Deserialize;

/// Number of entries, or feeds, shown on each page of a list.
pub(super) const PAGE_SIZE: u32 = 25;

/// Query parameters accepted by the index, feed, search and entry pages.
#[derive(Deserialize)]
pub(super) struct PageParams {
    /// The page of entries to show, or to link back to, counting from 1
    /// (default: 1).
    pub(super) page: Option<u32>,
    /// On an entry page, the feed whose page to link back to, rather than
    /// the index.
    pub(super) feed: Option<i64>,
    /// On an entry page, the tag whose page to link back to, rather than
    /// the index.
    pub(super) tag: Option<i64>,
    /// Also list entries tagged `system:read`, which are left out by
    /// default; on an entry page, whether the list it links back to does.
    pub(super) show_read: Option<bool>,
    /// On the search page, what to search for; on an entry page, the search
    /// whose results to link back to, rather than the index.
    pub(super) q: Option<String>,
    /// How search results are sorted: `newest` first, or by best match
    /// (the default).
    pub(super) sort: Option<String>,
}

impl PageParams {
    pub(super) fn page(&self) -> u32 {
        self.page.unwrap_or(1).max(1)
    }

    /// The list of entries an entry page links back to.
    pub(super) fn listing(&self) -> Listing {
        Listing {
            feed: self.feed,
            tag: self.tag,
            search: self.search(),
            page: self.page(),
            show_read: self.show_read(),
        }
    }

    /// The search given by the `q` and `sort` parameters, or `None` if `q`
    /// is missing or blank.
    pub(super) fn search(&self) -> Option<Search> {
        let query = self.q.as_deref()?.trim();
        (!query.is_empty()).then(|| Search {
            query: query.to_owned(),
            newest: self.sort.as_deref() == Some("newest"),
        })
    }

    pub(super) fn show_read(&self) -> bool {
        self.show_read.unwrap_or(false)
    }
}

/// A page of a list of entries: of the index, of a feed's page, of a tag's
/// page, or of search results. Entry pages link back to the listing they
/// were opened from.
#[derive(Clone)]
pub(super) struct Listing {
    /// The feed whose entries are listed, or `None` for the index.
    pub(super) feed: Option<i64>,
    /// The tag whose entries are listed, or `None` for the index.
    pub(super) tag: Option<i64>,
    /// The search whose results are listed, or `None` for the index. At
    /// most one of `feed`, `tag` and `search` is set.
    pub(super) search: Option<Search>,
    /// The page of the list, counting from 1.
    pub(super) page: u32,
    /// Whether entries tagged `system:read` are listed. Search results
    /// always list them, whatever this says.
    pub(super) show_read: bool,
}

/// A full-text search of the entries, as typed into the search box.
#[derive(Clone)]
pub(super) struct Search {
    /// What was searched for, trimmed and never empty. See [`fts_query`]
    /// for how it is understood.
    pub(super) query: String,
    /// Whether results are sorted newest first, rather than by best match.
    pub(super) newest: bool,
}

impl Search {
    /// The query parameters that give this search, already encoded.
    pub(super) fn query_params(&self) -> Vec<String> {
        let mut query = vec![format!("q={}", encode_path_segment(&self.query))];
        if self.newest {
            query.push("sort=newest".to_owned());
        }
        query
    }
}

impl Listing {
    /// The path of the list, without a page.
    pub(super) fn path(&self) -> String {
        match (self.feed, self.tag, &self.search) {
            (Some(id), _, _) => format!("/feeds/{id}"),
            (None, Some(id), _) => format!("/tags/{id}"),
            (None, None, Some(_)) => "/search".to_owned(),
            (None, None, None) => "/".to_owned(),
        }
    }

    /// The URL of this page of the list, escaped for use in an attribute.
    pub(super) fn href(&self) -> String {
        let page = (self.page > 1).then_some(self.page as usize);
        self.list_href(page, self.show_read)
    }

    /// The URL of page `page` of the list, escaped for use in an attribute.
    pub(super) fn page_href(&self, page: usize) -> String {
        self.list_href(Some(page), self.show_read)
    }

    /// The URL of the first page of these search results, sorted newest
    /// first if `newest` or by best match if not, escaped for use in an
    /// attribute.
    pub(super) fn sort_href(&self, newest: bool) -> String {
        let listing = Listing {
            search: self
                .search
                .clone()
                .map(|search| Search { newest, ..search }),
            ..self.clone()
        };
        listing.list_href(None, listing.show_read)
    }

    /// The URL of the first page of the list with read entries shown or
    /// not, the other way from this one, escaped for use in an attribute.
    pub(super) fn toggle_read_href(&self) -> String {
        self.list_href(None, !self.show_read)
    }

    /// The URL of page `page` of the list, or of its first page if `None`,
    /// listing read entries if `show_read`, escaped for use in an attribute.
    pub(super) fn list_href(&self, page: Option<usize>, show_read: bool) -> String {
        let mut query = Vec::new();
        if let Some(search) = &self.search {
            query.extend(search.query_params());
        }
        if let Some(page) = page {
            query.push(format!("page={page}"));
        }
        if show_read && self.search.is_none() {
            query.push("show_read=true".to_owned());
        }
        with_query(self.path(), &query)
    }

    /// The URL of entry `id`'s page, linking back to this page of the list,
    /// escaped for use in an attribute.
    pub(super) fn entry_href(&self, id: i64) -> String {
        let mut query = Vec::new();
        if let Some(feed) = self.feed {
            query.push(format!("feed={feed}"));
        }
        if let Some(tag) = self.tag {
            query.push(format!("tag={tag}"));
        }
        if let Some(search) = &self.search {
            query.extend(search.query_params());
        }
        if self.page > 1 {
            query.push(format!("page={}", self.page));
        }
        if self.show_read && self.search.is_none() {
            query.push("show_read=true".to_owned());
        }
        with_query(format!("/entries/{id}"), &query)
    }
}

/// `path` with the query parameters `query` (already encoded) appended,
/// escaped for use in an attribute.
pub(super) fn with_query(path: String, query: &[String]) -> String {
    if query.is_empty() {
        path
    } else {
        format!("{path}?{}", query.join("&amp;"))
    }
}

/// Render the note on the search page explaining how searches are
/// understood; see [`fts_query`].
pub(super) fn render_search_help() -> &'static str {
    "<p class=\"hint\">Entries match when every word appears in their title, \
     content or URL. Put words in &quot;quotes&quot; to match them as a phrase, \
     and end a word with * to match any word starting with it.</p>\n"
}

/// Turn `input`, as typed into the search box, into an FTS5 query for the
/// Kiki API, or `None` if it has no words to search for.
///
/// Rather than letting FTS5's own query syntax through — where a stray
/// quote, a hyphen or an apostrophe is a syntax error — every word becomes
/// its own quoted string, so that entries must contain all of them. Text in
/// double quotes is kept together as a phrase, and a `*` after a word or
/// phrase makes it match as a prefix. Words with no letters or digits are
/// dropped, since FTS5 would find nothing in them to match.
pub(super) fn fts_query(input: &str) -> Option<String> {
    let mut terms = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut term = String::new();
        if c == '"' {
            chars.next();
            term.extend(chars.by_ref().take_while(|&c| c != '"'));
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '"' {
                    break;
                }
                term.push(c);
                chars.next();
            }
        }
        let mut prefix = chars.next_if_eq(&'*').is_some();
        if let Some(stripped) = term.strip_suffix('*') {
            term = stripped.to_owned();
            prefix = true;
        }
        if term.chars().any(char::is_alphanumeric) {
            let star = if prefix { "*" } else { "" };
            terms.push(format!("\"{}\"{star}", term.replace('"', "\"\"")));
        }
    }
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// Render the links that sort a list of search results by best match or
/// newest first, the way they are sorted now shown without a link.
pub(super) fn render_sort(listing: &Listing) -> String {
    let newest = listing.search.as_ref().is_some_and(|s| s.newest);
    let option = |label: &str, sorts_newest: bool| {
        if sorts_newest == newest {
            format!("<strong aria-current=\"true\">{label}</strong>")
        } else {
            format!(
                "<a href=\"{}\">{label}</a>",
                listing.sort_href(sorts_newest)
            )
        }
    };
    format!(
        "<p class=\"sort\">Sort by {} &middot; {}</p>",
        option("best match", false),
        option("newest", true),
    )
}

/// Render the filter menu for a list of entries: a checkbox that shows read
/// entries, or hides them again. Ticking it reloads the first page of
/// `listing` with the other setting; the script in `page.js` follows the
/// checkbox's `data-href`, so that the page needs no form, and the page's
/// `Content-Security-Policy` can go on forbidding them.
pub(super) fn render_filter(listing: &Listing) -> String {
    let href = listing.toggle_read_href();
    format!(
        "<details class=\"filter\">\n<summary>Filter</summary>\n<div class=\"menu\">\n\
         <label><input type=\"checkbox\" class=\"filter-toggle\" data-href=\"{href}\"{}> Show read entries</label>\n\
         <noscript><a href=\"{href}\">{}</a></noscript>\n\
         </div>\n</details>\n",
        if listing.show_read { " checked" } else { "" },
        if listing.show_read { "Hide read entries" } else { "Show read entries" },
    )
}

/// Render the "page X of Y" line for page `page` of a list of `count`
/// items, with links to the pages before and after it, labelled with
/// `labels`. `href` gives the URL of a page of the list, escaped for use in
/// an attribute.
pub(super) fn render_pagination(
    count: usize,
    page: u32,
    href: impl Fn(usize) -> String,
    labels: (&str, &str),
) -> String {
    let (prev_label, next_label) = labels;
    let pages = count.div_ceil(PAGE_SIZE as usize).max(1);
    let page_usize = page as usize;

    let mut links = Vec::new();
    if page > 1 {
        // A page past the end links back to the last page, not to the
        // (equally empty) page before it.
        let prev = page_usize.min(pages + 1) - 1;
        links.push(format!(
            "<a href=\"{}\" rel=\"prev\">{prev_label}</a>",
            href(prev)
        ));
    }
    links.push(format!("Page {page} of {pages}"));
    if page_usize < pages {
        links.push(format!(
            "<a href=\"{}\" rel=\"next\">{next_label}</a>",
            href(page_usize + 1)
        ));
    }
    format!(
        "<nav class=\"pagination\">{}</nav>",
        links.join(" &middot; ")
    )
}
