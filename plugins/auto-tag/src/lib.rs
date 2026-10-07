//! Tag entries automatically: those whose fields match a regular expression, those from
//! given feeds, or both.
//!
//! Kiki's `auto-tag` plugin, built as WebAssembly by Kiki's `build.rs` (see
//! `plugins/Cargo.toml`) and installed as `plugins/auto-tag/plugin.wasm`. Versions before
//! 2.0.0 were written in Lua; this one takes the same config and tags the same entries.
//!
//! Config:
//!
//! * `rules`: a list of rules. An entry matching a rule is tagged with its tag; an entry
//!   matching several rules gets each of their tags.
//! * `rescan`: whether to apply the rules to the entries already stored when they change.
//!   Defaults to true.
//!
//! A rule is an object with:
//!
//! * `tag`: the tag to add: a user tag, or one of the system tags `system:read`,
//!   `system:saved` and `system:hidden`.
//! * `pattern`: an optional regular expression, in the syntax of the host's regexes
//!   (Lua's `kiki.regex`). Without it, the rule matches every entry from its feeds.
//! * `flags`: optional regex flags, such as `"i"` for case-insensitive.
//! * `fields`: the entry fields to match the pattern against: a list of names, from
//!   `title`, `url`, `content`, `authors`, `categories` and `guid`. The pattern matches if
//!   it matches any of them (for authors and categories, any one of the entry's). Missing
//!   or empty, it is `["title", "content"]`. A single name is accepted in place of a list,
//!   but the settings in `manifest.toml`, which the web UI and config API go by, only allow
//!   lists.
//! * `feeds`: an optional list of the feeds the rule applies to, each given by its id or by
//!   the URL it is fetched from. Missing or empty, the rule applies to every feed.
//!
//! A rule needs a pattern, feeds, or both: one with neither would tag every entry, and
//! fails to load.
//!
//! The plugin never removes a tag, so loosening or deleting a rule leaves the entries it
//! tagged tagged.
//!
//! Since the tags a plugin returns for an entry replace its user tags (see the plugin
//! documentation), a rule that matches an entry fetched again replaces the user tags it
//! was given since with the rule's tag.

use kiki_plugin::regex::Regex;
use kiki_plugin::{
    export_plugin, host, log, Entry, EventKind, FeedEvent, Level, Plugin, ScanOptions, ScanSummary,
};
use serde_json::{Map, Value};
use std::collections::{hash_map, HashMap, HashSet};

/// The system tags a rule may add. Any other name starting with `system:` is reserved.
const SYSTEM_TAGS: &[&str] = &["system:read", "system:saved", "system:hidden"];

/// The fields a rule's pattern can match.
#[derive(Clone, Copy)]
enum Field {
    Title,
    Url,
    Content,
    Authors,
    Categories,
    Guid,
}

impl Field {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "title" => Field::Title,
            "url" => Field::Url,
            "content" => Field::Content,
            "authors" => Field::Authors,
            "categories" => Field::Categories,
            "guid" => Field::Guid,
            _ => return None,
        })
    }

    /// The values of the field in `entry`.
    fn values(self, entry: &Entry) -> &[String] {
        match self {
            Field::Title => std::slice::from_ref(&entry.title),
            Field::Url => entry.url.as_slice(),
            Field::Content => entry.content.as_slice(),
            Field::Authors => &entry.authors,
            Field::Categories => &entry.categories,
            Field::Guid => std::slice::from_ref(&entry.guid),
        }
    }
}

const DEFAULT_FIELDS: &[Field] = &[Field::Title, Field::Content];

/// The feeds a rule applies to, by id and by URL.
struct Feeds {
    ids: HashSet<i64>,
    urls: HashSet<String>,
}

/// A compiled rule.
struct Rule {
    tag: String,
    /// The pattern, and the fields it matches; `None` for a rule without one.
    pattern: Option<(Regex, Vec<Field>)>,
    /// The feeds the rule applies to; `None` for every feed.
    feeds: Option<Feeds>,
    /// Where in the config the rule is, as `rules[<n>]`.
    place: String,
}

fn fail(place: &str, message: &str) -> String {
    format!("auto-tag: {place}: {message}")
}

/// `value` as a list: a list, or an empty object (an empty Lua table, as configs written
/// for the Lua plugin could hold).
fn as_list(value: &Value) -> Option<&[Value]> {
    match value {
        Value::Array(list) => Some(list),
        Value::Object(map) if map.is_empty() => Some(&[]),
        _ => None,
    }
}

/// `value` as an integer: an integer, or a float with an integer's value.
fn as_integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        let f = value.as_f64()?;
        // i64::MAX as f64 rounds up to 2^63, which is out of range.
        (f.fract() == 0.0 && f >= i64::MIN as f64 && f < i64::MAX as f64).then_some(f as i64)
    })
}

fn compile_tag(place: &str, tag: Option<&Value>) -> Result<String, String> {
    let tag = match tag {
        Some(Value::String(tag)) if !tag.is_empty() => tag,
        _ => return Err(fail(place, "'tag' must be a tag name")),
    };
    if tag.starts_with("system:") && !SYSTEM_TAGS.contains(&tag.as_str()) {
        return Err(fail(
            place,
            &format!(
                "unknown system tag {tag:?}; expected system:read, system:saved or system:hidden"
            ),
        ));
    }
    Ok(tag.clone())
}

fn compile_pattern(
    place: &str,
    rule: &Map<String, Value>,
) -> Result<Option<(Regex, Vec<Field>)>, String> {
    let pattern = match rule.get("pattern") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(pattern)) if pattern.is_empty() => return Ok(None),
        Some(Value::String(pattern)) => pattern,
        Some(_) => return Err(fail(place, "'pattern' must be a string")),
    };

    let names = match rule.get("fields") {
        None | Some(Value::Null) => vec![],
        Some(Value::String(name)) => vec![Value::String(name.clone())],
        Some(fields) => as_list(fields)
            .ok_or_else(|| fail(place, "'fields' must be a field name or a list of them"))?
            .to_vec(),
    };
    let fields = if names.is_empty() {
        DEFAULT_FIELDS.to_vec()
    } else {
        names
            .iter()
            .map(|name| {
                name.as_str().and_then(Field::parse).ok_or_else(|| {
                    let name = match name {
                        Value::String(name) => format!("{name:?}"),
                        other => other.to_string(),
                    };
                    fail(
                        place,
                        &format!(
                            "unknown field {name}; expected title, url, content, authors, \
                             categories or guid"
                        ),
                    )
                })
            })
            .collect::<Result<_, _>>()?
    };

    let flags = match rule.get("flags") {
        None | Some(Value::Null) => "",
        Some(Value::String(flags)) => flags,
        Some(_) => return Err(fail(place, "'flags' must be a string")),
    };
    let re = Regex::compile(pattern, flags).map_err(|e| fail(place, &e))?;
    Ok(Some((re, fields)))
}

fn compile_feeds(place: &str, feeds: Option<&Value>) -> Result<Option<Feeds>, String> {
    let message = "'feeds' must be a list of feed ids or URLs";
    let list = match feeds {
        None | Some(Value::Null) => return Ok(None),
        Some(feeds) => as_list(feeds).ok_or_else(|| fail(place, message))?,
    };
    if list.is_empty() {
        return Ok(None);
    }
    let (mut ids, mut urls) = (HashSet::new(), HashSet::new());
    for feed in list {
        match (feed, as_integer(feed)) {
            (Value::String(url), _) => {
                urls.insert(url.clone());
            }
            (_, Some(id)) => {
                ids.insert(id);
            }
            _ => return Err(fail(place, message)),
        }
    }
    Ok(Some(Feeds { ids, urls }))
}

fn compile_rule(place: String, rule: &Value) -> Result<Rule, String> {
    let rule = rule
        .as_object()
        .ok_or_else(|| fail(&place, "a rule must be a table"))?;
    let tag = compile_tag(&place, rule.get("tag"))?;
    let pattern = compile_pattern(&place, rule)?;
    let feeds = compile_feeds(&place, rule.get("feeds"))?;
    if pattern.is_none() && feeds.is_none() {
        return Err(fail(&place, "a rule needs a 'pattern', 'feeds', or both"));
    }
    Ok(Rule {
        tag,
        pattern,
        feeds,
        place,
    })
}

/// Whether `a` and `b`, the rules as configured and as last applied, are the same.
///
/// The Lua versions of the plugin stored the rules as Lua kept them, so this compares
/// them as Lua would have: an empty object is the same as an empty list, a key whose value
/// is `null` is the same as one that is missing, and numbers are compared by value.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(x), Some(y)) => x == y,
            _ => x.as_f64() == y.as_f64(),
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y))
        }
        (Value::Array(list), Value::Object(map)) | (Value::Object(map), Value::Array(list)) => {
            list.is_empty() && map.values().all(Value::is_null)
        }
        (Value::Object(x), Value::Object(y)) => {
            let covers = |x: &Map<String, Value>, y: &Map<String, Value>| {
                x.iter()
                    .all(|(k, v)| same(v, y.get(k).unwrap_or(&Value::Null)))
            };
            covers(x, y) && covers(y, x)
        }
        (a, b) => a == b,
    }
}

struct AutoTag {
    rules: Vec<Rule>,
    /// Whether to apply the rules to stored entries when they change.
    rescan: bool,
    /// The rules as configured, kept in the store once applied to stored entries.
    applied: Value,
    feed_urls: FeedUrls,
    /// The scan applying the rules to stored entries, if one is going, and how many
    /// entries it has tagged so far.
    scan: Option<(u64, u64)>,
}

/// The URL of each feed looked up so far, by feed id; `None` for a feed with no URL, or
/// that does not exist.
#[derive(Default)]
struct FeedUrls(HashMap<i64, Option<String>>);

impl FeedUrls {
    /// The URL of feed `feed_id`, looked up once per feed. A failed lookup is logged and
    /// not remembered, so that the next entry tries again.
    fn get(&mut self, feed_id: i64) -> Option<&str> {
        let url = match self.0.entry(feed_id) {
            hash_map::Entry::Occupied(url) => url.into_mut(),
            hash_map::Entry::Vacant(slot) => match host::get_feed(feed_id) {
                Ok(feed) => slot.insert(feed.and_then(|feed| feed.url)),
                Err(e) => {
                    log(
                        Level::Warn,
                        &format!("auto-tag: looking up feed {feed_id}: {e}"),
                    );
                    return None;
                }
            },
        };
        url.as_deref()
    }

    fn forget(&mut self, feed_id: i64) {
        self.0.remove(&feed_id);
    }
}

impl Rule {
    /// Whether the rule applies to entries from feed `feed_id`.
    fn applies(&self, feed_id: i64, urls: &mut FeedUrls) -> bool {
        match &self.feeds {
            None => true,
            Some(feeds) => {
                feeds.ids.contains(&feed_id)
                    || (!feeds.urls.is_empty()
                        && urls
                            .get(feed_id)
                            .is_some_and(|url| feeds.urls.contains(url)))
            }
        }
    }

    /// Whether the rule's pattern, if it has one, matches `entry`.
    fn matches(&self, entry: &Entry) -> bool {
        self.pattern.as_ref().is_none_or(|(re, fields)| {
            fields
                .iter()
                .any(|field| field.values(entry).iter().any(|v| re.is_match(v)))
        })
    }
}

impl AutoTag {
    /// The tags of the rules `entry` matches, in rule order, without repeats.
    fn tags_for(&mut self, entry: &Entry) -> Vec<String> {
        let mut tags: Vec<String> = Vec::new();
        for rule in &self.rules {
            if !tags.contains(&rule.tag)
                && rule.applies(entry.feed_id, &mut self.feed_urls)
                && rule.matches(entry)
            {
                log(
                    Level::Debug,
                    &format!(
                        "auto-tag: tagging {:?} {}: {} matched",
                        entry.guid, rule.tag, rule.place
                    ),
                );
                tags.push(rule.tag.clone());
            }
        }
        tags
    }

    /// Records the rules as applied to the stored entries.
    fn record_applied(&self) {
        if let Err(e) = host::set("rules", &self.applied) {
            log(
                Level::Error,
                &format!("auto-tag: recording the rules as applied: {e}"),
            );
        }
    }
}

impl Plugin for AutoTag {
    const EVENTS: &'static [EventKind] = &[
        EventKind::EntryIngest,
        EventKind::FeedRemoved,
        EventKind::PluginLoad,
    ];

    fn new(config: &str) -> Result<Self, String> {
        let config: Map<String, Value> =
            serde_json::from_str(config).map_err(|e| format!("auto-tag: invalid config: {e}"))?;
        let applied = match config.get("rules") {
            None | Some(Value::Null) => Value::Array(vec![]),
            Some(rules) => rules.clone(),
        };
        // Compiled here, so that a bad rule fails when the plugin loads rather than on
        // every entry.
        let rules = as_list(&applied)
            .ok_or_else(|| fail("rules", "must be a list of rules"))?
            .iter()
            .enumerate()
            .map(|(i, rule)| compile_rule(format!("rules[{}]", i + 1), rule))
            .collect::<Result<_, _>>()?;
        Ok(AutoTag {
            rules,
            rescan: config.get("rescan") != Some(&Value::Bool(false)),
            applied,
            feed_urls: FeedUrls::default(),
            scan: None,
        })
    }

    fn on_entry_ingest(&mut self, mut entry: Entry) -> Option<Entry> {
        for tag in self.tags_for(&entry) {
            // An earlier plugin may have added the tag already.
            if !entry.tags.contains(&tag) {
                entry.tags.push(tag);
            }
        }
        Some(entry)
    }

    // A feed's id may be given to a new feed once it is removed.
    fn on_feed_removed(&mut self, feed: FeedEvent) {
        self.feed_urls.forget(feed.id);
    }

    // When the rules change, apply them to the entries already stored. The rules last
    // applied are kept in the plugin's store, so that restarting the server, or reloading
    // plugins for some other reason, does not rescan: that would tag again any entry the
    // user untagged.
    //
    // The rules are only recorded as applied once the scan has gone through every entry.
    // A scan cut short, by a reload or the server stopping, runs again from the start on
    // the next load.
    fn on_plugin_load(&mut self) {
        if !self.rescan {
            return;
        }
        match host::get::<Value>("rules") {
            Ok(Some(stored)) if same(&stored, &self.applied) => return,
            Ok(_) => {}
            Err(e) => {
                log(
                    Level::Error,
                    &format!("auto-tag: reading the rules last applied: {e}"),
                );
                return;
            }
        }
        if self.rules.is_empty() {
            self.record_applied();
            return;
        }
        log(
            Level::Info,
            "auto-tag: rules changed; applying them to stored entries",
        );
        let options = ScanOptions {
            feed_id: None,
            since: None,
            include_hidden: false,
        };
        match host::start_scan(options) {
            Ok(id) => self.scan = Some((id, 0)),
            Err(e) => log(
                Level::Error,
                &format!("auto-tag: applying the rules to stored entries: {e}"),
            ),
        }
    }

    fn on_scan_entry(&mut self, scan: u64, entry: Entry) -> Option<Entry> {
        let id = entry.id?;
        if self.scan.is_none_or(|(current, _)| current != scan) {
            return None;
        }
        let mut added = false;
        for tag in self.tags_for(&entry) {
            match host::tag_entry(id, &tag) {
                Ok(true) => added = true,
                Ok(false) => {}
                Err(e) => log(
                    Level::Warn,
                    &format!("auto-tag: tagging entry {id} {tag}: {e}"),
                ),
            }
        }
        if let Some((_, tagged)) = &mut self.scan {
            *tagged += u64::from(added);
        }
        None
    }

    fn on_scan_done(&mut self, scan: u64, summary: ScanSummary) {
        let Some((_, tagged)) = self.scan.take_if(|(current, _)| *current == scan) else {
            return;
        };
        log(
            Level::Info,
            &format!(
                "auto-tag: applied the rules to {} stored entries, tagging {tagged}",
                summary.scanned
            ),
        );
        self.record_applied();
    }
}

export_plugin!(AutoTag);
