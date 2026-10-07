//! Strip tracking parameters, such as `utm_source` or `fbclid`, from the URLs in new
//! entries, and tracking pixels from their content, and keep Kiki from downloading any
//! images at all for the feeds that ask for it.
//!
//! Kiki's `privacy` plugin, built as WebAssembly by Kiki's `build.rs` (see
//! `plugins/Cargo.toml`) and installed as `plugins/privacy/plugin.wasm`. Versions before
//! 2.0.0 were written in Lua; this one takes the same config and cleans entries the same
//! way.
//!
//! Config:
//!
//! * `params`: a list of the query parameters to remove. A name ending in `*` removes
//!   every parameter whose name starts with the rest of it, so `"utm_*"` removes
//!   `utm_source`, `utm_medium` and so on. Names are matched ignoring case, and as they
//!   are written in the URL (percent-encoded names are not decoded).
//! * `content`: whether to also clean the links in each entry's content: the values of
//!   its `href` and `src` attributes. Defaults to true.
//! * `pixels`: whether to remove tracking pixels from each entry's content: images
//!   declared no bigger than 1x1 (both their `width` and `height` attributes are 0 or 1),
//!   and images from the trackers in `trackers`. Defaults to true.
//! * `trackers`: a list of the image sources whose images are always removed, when
//!   `pixels` is true. Each is a host name, which may start with `*.` to match the domain
//!   and all of its subdomains, and may be followed by the start of a path:
//!   `"medium.com/_/stat"` removes images from `https://medium.com/_/stat?event=...` but
//!   not other images from medium.com. Host names are matched ignoring case, paths as they
//!   are written.
//! * `skip_assets`: a list of feeds, each given by its id or by the URL it is fetched
//!   from, whose entries' images and enclosures are never downloaded into Kiki's asset
//!   cache, so that the sites they are served from never hear from Kiki. The entries are
//!   stored as they are; only the downloads are skipped. Empty by default.
//!
//! The parameters are removed from an entry's query string, and from its fragment when
//! that is written like one (`#xtor=RSS-1`). Every other part of a URL is left as it was,
//! and a URL with nothing to remove is not touched at all. A query or fragment left empty
//! is dropped, along with its `?` or `#`. In content, `&amp;` is understood as a separator
//! too.
//!
//! Removing a tracking pixel removes its whole `<img>` element, so the image is neither
//! shown nor downloaded into Kiki's asset cache.
//!
//! Only entries as they are fetched are cleaned: plugins cannot change the URL or content
//! of entries already stored, so adding a parameter or a tracker to the list does not
//! clean the entries downloaded before.

use kiki_plugin::{
    export_plugin, host, log, Entry, EventKind, FeedEvent, FetchSuccess, Level, Plugin,
};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

/// An entry in `trackers`.
struct Tracker {
    /// The host name to match, lowercase.
    host: String,
    /// Whether subdomains of `host` match too (the tracker was written `*.host`).
    subdomains: bool,
    /// The start of the path to match, possibly empty.
    path: String,
}

impl Tracker {
    /// Parse `tracker`: a host name, optionally starting with `*.`, optionally followed by
    /// a path starting with `/` and holding no whitespace.
    fn parse(tracker: &str) -> Option<Self> {
        let (subdomains, rest) = match tracker.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, tracker),
        };
        let host_len = rest
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            .count();
        let (host, path) = rest.split_at(host_len);
        let path_ok = path.is_empty() || (path.starts_with('/') && !path.bytes().any(is_space));
        (!host.is_empty() && path_ok).then(|| Tracker {
            host: host.to_ascii_lowercase(),
            subdomains,
            path: path.to_string(),
        })
    }

    /// Whether an image from `host`, normalized as [`url_host`] has it, with `rest` after
    /// it, is from this tracker.
    fn matches(&self, host: &str, rest: &str) -> bool {
        let host_matches = host == self.host
            || (self.subdomains
                && host
                    .strip_suffix(self.host.as_str())
                    .is_some_and(|sub| sub.ends_with('.')));
        host_matches && rest.starts_with(self.path.as_str())
    }
}

struct Privacy {
    /// The exact names in `params`, lowercase.
    names: HashSet<String>,
    /// The prefixes given by the names ending in `*` in `params`, lowercase.
    prefixes: Vec<String>,
    /// Whether to clean the links in entries' content.
    content: bool,
    /// Whether to remove tracking pixels from entries' content.
    pixels: bool,
    trackers: Vec<Tracker>,
    /// The feeds in `skip_assets` given by id.
    skip_ids: HashSet<i64>,
    /// The feeds in `skip_assets` given by URL.
    skip_urls: HashSet<String>,
    /// Whether each feed looked up is in `skip_assets` by URL, looked up once per feed.
    skips: HashMap<i64, bool>,
}

/// Reads config key `key` as a list, failing with `what` if it is anything else.
fn list<'a>(config: &'a Map<String, Value>, key: &str, what: &str) -> Result<&'a [Value], String> {
    match config.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        // An empty Lua table, as configs written for the Lua plugin could hold.
        Some(Value::Object(map)) if map.is_empty() => Ok(&[]),
        Some(Value::Array(list)) => Ok(list),
        Some(_) => Err(format!("privacy: '{key}' must be {what}")),
    }
}

/// `value` as a feed id: an integer, or a float with an integer's value.
fn feed_id(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        let f = value.as_f64()?;
        // i64::MAX as f64 rounds up to 2^63, which is out of range.
        (f.fract() == 0.0 && f >= i64::MIN as f64 && f < i64::MAX as f64).then_some(f as i64)
    })
}

impl Privacy {
    fn from_config(config: &Map<String, Value>) -> Result<Self, String> {
        let mut names = HashSet::new();
        let mut prefixes = Vec::new();
        for (i, param) in list(config, "params", "a list of parameter names")?
            .iter()
            .enumerate()
        {
            let param = match param.as_str() {
                Some(param) if !param.is_empty() && param != "*" => param.to_ascii_lowercase(),
                _ => {
                    return Err(format!(
                        "privacy: params[{}] must be a parameter name, or a prefix followed by \
                         '*'",
                        i + 1
                    ))
                }
            };
            match param.strip_suffix('*') {
                Some(prefix) => prefixes.push(prefix.to_string()),
                None => {
                    names.insert(param);
                }
            }
        }

        let mut trackers = Vec::new();
        for (i, tracker) in list(config, "trackers", "a list of host names")?
            .iter()
            .enumerate()
        {
            let tracker = tracker.as_str().and_then(Tracker::parse).ok_or_else(|| {
                format!(
                    "privacy: trackers[{}] must be a host name, optionally starting with '*.' \
                     and followed by a path",
                    i + 1
                )
            })?;
            trackers.push(tracker);
        }

        let mut skip_ids = HashSet::new();
        let mut skip_urls = HashSet::new();
        for (i, feed) in list(config, "skip_assets", "a list of feed ids or URLs")?
            .iter()
            .enumerate()
        {
            match (feed.as_str(), feed_id(feed)) {
                (Some(url), _) if !url.is_empty() => {
                    skip_urls.insert(url.to_string());
                }
                (None, Some(id)) => {
                    skip_ids.insert(id);
                }
                _ => {
                    return Err(format!(
                        "privacy: skip_assets[{}] must be a feed id or URL",
                        i + 1
                    ))
                }
            }
        }

        Ok(Privacy {
            names,
            prefixes,
            content: config.get("content") != Some(&Value::Bool(false)),
            pixels: config.get("pixels") != Some(&Value::Bool(false)),
            trackers,
            skip_ids,
            skip_urls,
            skips: HashMap::new(),
        })
    }

    fn is_tracking(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        self.names.contains(&name) || self.prefixes.iter().any(|p| name.starts_with(p.as_str()))
    }

    /// Removes the tracking parameters from `s`, a query string or a fragment, without its
    /// `?` or `#`. Returns `None` if there are none, and otherwise what is left, or `None`
    /// inside if nothing is.
    ///
    /// Parameters are separated by `&`, or by `&amp;` as in HTML. Each one kept keeps the
    /// separator that came before it.
    fn strip_params(&self, s: &str) -> Option<Option<String>> {
        // Each part kept, with the separator before it.
        let mut parts = Vec::new();
        let mut removed = false;
        let mut rest = s;
        let mut sep = "";
        loop {
            let (part, next) = match rest.split_once('&') {
                Some((part, next)) => (part, Some(next)),
                None => (rest, None),
            };
            let name = part.split_once('=').map_or(part, |(name, _)| name);
            if !part.is_empty() && self.is_tracking(name) {
                removed = true;
            } else {
                parts.push((sep, part));
            }
            let Some(next) = next else { break };
            (sep, rest) = match next.strip_prefix("amp;") {
                Some(next) => ("&amp;", next),
                None => ("&", next),
            };
        }
        if !removed {
            return None;
        }

        let mut out = String::with_capacity(s.len());
        // Empty parts (as in `a=1&&b=2`) would leave stray separators.
        for (sep, part) in parts.into_iter().filter(|(_, part)| !part.is_empty()) {
            if !out.is_empty() {
                out.push_str(sep);
            }
            out.push_str(part);
        }
        Some((!out.is_empty()).then_some(out))
    }

    /// `url` with its tracking parameters removed, or `None` if it has none.
    fn clean_url(&self, url: &str) -> Option<String> {
        let (base, fragment) = match url.split_once('#') {
            Some((base, fragment)) => (base, Some(fragment)),
            None => (url, None),
        };
        let (path, query) = match base.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (base, None),
        };

        let new_query = query.and_then(|q| self.strip_params(q));
        let new_fragment = fragment.and_then(|f| self.strip_params(f));
        if new_query.is_none() && new_fragment.is_none() {
            return None;
        }
        let query = new_query.map_or(query.map(Cow::Borrowed), |q| q.map(Cow::Owned));
        let fragment = new_fragment.map_or(fragment.map(Cow::Borrowed), |f| f.map(Cow::Owned));

        let mut out = String::with_capacity(url.len());
        out.push_str(path);
        if let Some(query) = query {
            out.push('?');
            out.push_str(&query);
        }
        if let Some(fragment) = fragment {
            out.push('#');
            out.push_str(&fragment);
        }
        Some(out)
    }

    /// `html` with the tracking parameters removed from the URLs in its `href` and `src`
    /// attributes, or `None` if they have none.
    fn clean_content(&self, html: &str) -> Option<String> {
        let mut html = Cow::Borrowed(html);
        for name in ["href", "src"] {
            for quoted in [true, false] {
                if let Some(cleaned) = self.clean_attribute(&html, name, quoted) {
                    html = Cow::Owned(cleaned);
                }
            }
        }
        match html {
            Cow::Owned(html) => Some(html),
            Cow::Borrowed(_) => None,
        }
    }

    /// `html` with the tracking parameters removed from the values of the attributes named
    /// `name`, quoted or not as `quoted` says, or `None` if they have none.
    ///
    /// Attributes are found by their text alone, without parsing the HTML: an attribute's
    /// name must follow whitespace, so that `data-src`, say, is left alone. The HTML is
    /// read from left to right, and carried on after each attribute found.
    fn clean_attribute(&self, html: &str, name: &str, quoted: bool) -> Option<String> {
        let bytes = html.as_bytes();
        let mut out: Option<String> = None;
        // How much of `html` is in `out`.
        let mut copied = 0;
        let mut pos = 0;
        while let Some(start) = find(bytes, pos, is_space) {
            pos = start + 1;
            let Some((value_start, value_end, end)) = attribute_value(bytes, start, name, quoted)
            else {
                continue;
            };
            pos = end;
            let (Some(value), Some(before)) = (
                html.get(value_start..value_end),
                html.get(copied..value_start),
            ) else {
                continue;
            };
            if let Some(cleaned) = self.clean_url(value) {
                let out = out.get_or_insert_with(|| String::with_capacity(html.len()));
                out.push_str(before);
                out.push_str(&cleaned);
                copied = value_end;
            }
        }
        let mut out = out?;
        out.push_str(html.get(copied..).unwrap_or_default());
        Some(out)
    }

    /// Whether `src`, an image's source, is from one of the `trackers`.
    fn is_tracker(&self, src: &str) -> bool {
        url_host(src)
            .is_some_and(|(host, rest)| self.trackers.iter().any(|t| t.matches(&host, rest)))
    }

    /// Whether an image with the attributes `attrs` is a tracking pixel.
    fn is_pixel(&self, attrs: &ImageAttributes) -> bool {
        (is_tiny(attrs.width) && is_tiny(attrs.height))
            || attrs.src.is_some_and(|src| self.is_tracker(src))
    }

    /// `html` with its tracking pixels' `<img>` elements removed, or `None` if it has none.
    fn remove_pixels(&self, html: &str) -> Option<String> {
        let bytes = html.as_bytes();
        let mut out: Option<String> = None;
        // How much of `html` is in `out`.
        let mut copied = 0;
        let mut pos = 0;
        while let Some(start) = find(bytes, pos, |b| b == b'<') {
            let name_end = start + 4;
            let is_img = bytes
                .get(start + 1..name_end)
                .is_some_and(|name| name.eq_ignore_ascii_case(b"img"));
            if !is_img {
                pos = start + 1;
                continue;
            }
            // Not an <img> tag (say, <imgx>), one to keep, or one left unclosed: carry on
            // after its name.
            pos = name_end;
            if !bytes
                .get(name_end)
                .is_some_and(|&b| is_space(b) || b == b'/' || b == b'>')
            {
                continue;
            }
            let Some((attrs, close)) = read_tag(html, name_end) else {
                continue;
            };
            if self.is_pixel(&attrs) {
                let out = out.get_or_insert_with(|| String::with_capacity(html.len()));
                out.push_str(html.get(copied..start).unwrap_or_default());
                copied = close + 1;
                pos = close + 1;
            }
        }
        let mut out = out?;
        out.push_str(html.get(copied..).unwrap_or_default());
        Some(out)
    }

    /// Whether feed `feed_id` is in `skip_assets`, by id or by URL.
    fn skips_assets(&mut self, feed_id: i64) -> bool {
        if self.skip_ids.contains(&feed_id) {
            return true;
        }
        if self.skip_urls.is_empty() {
            return false;
        }
        if let Some(&skip) = self.skips.get(&feed_id) {
            return skip;
        }
        match host::get_feed(feed_id) {
            Ok(feed) => {
                let skip = feed
                    .and_then(|feed| feed.url)
                    .is_some_and(|url| self.skip_urls.contains(&url));
                self.skips.insert(feed_id, skip);
                skip
            }
            Err(e) => {
                // Not remembered, so that the next entry tries again.
                log(
                    Level::Warn,
                    &format!("privacy: looking up feed {feed_id}: {e}"),
                );
                false
            }
        }
    }
}

/// Whether `b` is whitespace as the Lua plugin's patterns had it (`%s`): ASCII
/// whitespace, and the vertical tab.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

/// `s` without the whitespace ([`is_space`]) at its start.
fn trim_space_start(s: &str) -> &str {
    s.trim_start_matches(|c: char| u8::try_from(c).is_ok_and(is_space))
}

/// The position of the first byte at or after `from` in `bytes` for which `pred` holds.
fn find(bytes: &[u8], from: usize, pred: impl Fn(u8) -> bool) -> Option<usize> {
    let i = bytes.get(from..)?.iter().position(|&b| pred(b))?;
    Some(from + i)
}

/// If the whitespace at `start` in `bytes` is followed by the attribute `name` (in any
/// case), `=` (with any whitespace around it) and a value, quoted or not as `quoted` says:
/// where the value starts and ends, and where the attribute ends.
///
/// A quoted value runs up to the next of the same quote, which must be there. An unquoted
/// value starts with anything but whitespace, a quote, `` ` ``, `=`, `<` or `>`, and runs
/// up to whitespace or `>`.
fn attribute_value(
    bytes: &[u8],
    start: usize,
    name: &str,
    quoted: bool,
) -> Option<(usize, usize, usize)> {
    let name_end = start + 1 + name.len();
    if !bytes
        .get(start + 1..name_end)?
        .eq_ignore_ascii_case(name.as_bytes())
    {
        return None;
    }
    let equals = find(bytes, name_end, |b| !is_space(b))?;
    if bytes.get(equals) != Some(&b'=') {
        return None;
    }
    let value = find(bytes, equals + 1, |b| !is_space(b))?;
    let first = *bytes.get(value)?;
    if quoted {
        if first != b'"' && first != b'\'' {
            return None;
        }
        let close = find(bytes, value + 1, |b| b == first)?;
        Some((value + 1, close, close + 1))
    } else {
        if matches!(first, b'"' | b'\'' | b'`' | b'=' | b'<' | b'>') {
            return None;
        }
        let end = find(bytes, value, |b| is_space(b) || b == b'>').unwrap_or(bytes.len());
        Some((value, end, end))
    }
}

/// The host of `src`, an absolute or scheme-relative URL, without any credentials, port
/// or trailing dot, lowercase; and the rest of `src` after it. `None` if `src` has no
/// host.
fn url_host(src: &str) -> Option<(String, &str)> {
    let src = trim_space_start(src);
    let after = match src.strip_prefix("//") {
        Some(after) => after,
        None => {
            // A scheme: a letter, then letters, digits, `+`, `.` or `-`.
            if !src.starts_with(|c: char| c.is_ascii_alphabetic()) {
                return None;
            }
            let scheme_len = src
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
                .count();
            src.get(scheme_len..)?.strip_prefix("://")?
        }
    };
    let host_len = after.find(['/', '?', '#']).unwrap_or(after.len());
    let (host, rest) = after.split_at(host_len);

    // Drop any credentials and port, and the trailing dot of a fully-qualified name.
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let host = host
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .strip_suffix(':')
        .unwrap_or(host);
    let host = host.strip_suffix('.').unwrap_or(host);
    Some((host.to_ascii_lowercase(), rest))
}

/// Whether `value`, a width or height attribute, is at most one pixel: a number, possibly
/// followed by `px`, with any whitespace around them.
fn is_tiny(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let value = trim_space_start(value);
    let number_len = value
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b'.')
        .count();
    let (number, unit) = value.split_at(number_len);
    let unit = trim_space_start(unit);
    let unit = unit.strip_prefix(['p', 'P']).unwrap_or(unit);
    let unit = unit.strip_prefix(['x', 'X']).unwrap_or(unit);
    if !unit.bytes().all(is_space) {
        return false;
    }
    // Digits and dots only, so this parses as Lua's `tonumber` would.
    number.parse::<f64>().is_ok_and(|n| n <= 1.0)
}

/// The attributes of an `<img>` tag that tell whether it is a tracking pixel: the first of
/// each given, if any.
#[derive(Default)]
struct ImageAttributes<'a> {
    width: Option<&'a str>,
    height: Option<&'a str>,
    src: Option<&'a str>,
}

/// Reads the attributes of the tag in `html` whose name ends just before `pos`. Returns
/// them, and the position of the `>` closing the tag; or `None`, if the tag is not closed.
fn read_tag(html: &str, mut pos: usize) -> Option<(ImageAttributes<'_>, usize)> {
    let bytes = html.as_bytes();
    let mut attrs = ImageAttributes::default();
    loop {
        pos = find(bytes, pos, |b| !is_space(b) && b != b'/')?;
        if bytes.get(pos) == Some(&b'>') {
            return Some((attrs, pos));
        }
        let name_end = find(bytes, pos + 1, |b| {
            is_space(b) || matches!(b, b'/' | b'>' | b'=')
        })
        .unwrap_or(bytes.len());
        let name = html.get(pos..name_end)?;
        let mut value = "";
        pos = find(bytes, name_end, |b| !is_space(b))?;
        if bytes.get(pos) == Some(&b'=') {
            pos = find(bytes, pos + 1, |b| !is_space(b))?;
            let quote = *bytes.get(pos)?;
            if quote == b'"' || quote == b'\'' {
                let close = find(bytes, pos + 1, |b| b == quote)?;
                value = html.get(pos + 1..close)?;
                pos = close + 1;
            } else {
                let value_end =
                    find(bytes, pos, |b| is_space(b) || b == b'>').unwrap_or(bytes.len());
                value = html.get(pos..value_end)?;
                pos = value_end;
            }
        }
        let slot = if name.eq_ignore_ascii_case("width") {
            &mut attrs.width
        } else if name.eq_ignore_ascii_case("height") {
            &mut attrs.height
        } else if name.eq_ignore_ascii_case("src") {
            &mut attrs.src
        } else {
            continue;
        };
        slot.get_or_insert(value);
    }
}

impl Plugin for Privacy {
    const EVENTS: &'static [EventKind] = &[
        EventKind::EntryIngest,
        EventKind::FetchSuccess,
        EventKind::FeedRemoved,
    ];

    fn new(config: &str) -> Result<Self, String> {
        let config: Map<String, Value> =
            serde_json::from_str(config).map_err(|e| format!("privacy: invalid config: {e}"))?;
        Privacy::from_config(&config)
    }

    fn on_entry_ingest(&mut self, mut entry: Entry) -> Option<Entry> {
        if let Some(cleaned) = entry.url.as_deref().and_then(|url| self.clean_url(url)) {
            entry.url = Some(cleaned);
        }
        if self.pixels {
            if let Some(cleaned) = entry.content.as_deref().and_then(|c| self.remove_pixels(c)) {
                entry.content = Some(cleaned);
            }
        }
        if self.content {
            if let Some(cleaned) = entry.content.as_deref().and_then(|c| self.clean_content(c)) {
                entry.content = Some(cleaned);
            }
        }
        if self.skips_assets(entry.feed_id) {
            entry.cache_assets = false;
        }
        Some(entry)
    }

    // A feed's URL changes when it is permanently redirected, and a removed feed's id may
    // be given to a new feed.
    fn on_fetch_success(&mut self, event: FetchSuccess) {
        self.skips.remove(&event.feed_id);
    }

    fn on_feed_removed(&mut self, feed: FeedEvent) {
        self.skips.remove(&feed.id);
    }
}

export_plugin!(Privacy);
