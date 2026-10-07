//! Hide entries whose fields match, or fail to match, regular expressions.
//!
//! Config:
//!
//! * `exclude`: a list of rules. An entry matching any of them is hidden.
//! * `include`: a list of rules. An entry whose feed has include rules, and that matches
//!   none of them, is hidden.
//! * `rescan`: whether to apply the rules to the entries already stored when they change.
//!   Defaults to true.
//!
//! A rule is an object with:
//!
//! * `pattern`: the regular expression, in the syntax of `kiki.regex`.
//! * `flags`: optional `kiki.regex` flags, such as `"i"` for case-insensitive.
//! * `fields`: the entry fields to match: a list of names, from title, url, content,
//!   authors, categories and guid. A rule matches if the pattern matches any of them (for
//!   authors and categories, any one of the entry's). Missing or empty, it is
//!   `["title", "content"]`. A single name is accepted in place of a list.
//! * `feeds`: optional list of the feeds the rule applies to, each given by its id or by
//!   the URL it is fetched from. Without it, the rule applies to every feed.
//!
//! Hidden entries are tagged `system:hidden`. The filter never unhides an entry.

use kiki_plugin::host;
use kiki_plugin::regex::{Regex, RegexSet};
use kiki_plugin::{log, plugin, Entry, FeedEvent, Level, Plugin, ScanOptions};
use serde_json::{Map, Value};
use std::collections::{hash_map, HashMap, HashSet};

const HIDDEN: &str = "system:hidden";

/// The key the rules last applied to the stored entries are kept under.
const RULES_KEY: &str = "rules";

/// An entry field a rule can match.
#[derive(Clone, Copy, PartialEq)]
enum Field {
    Title,
    Url,
    Content,
    Guid,
    Authors,
    Categories,
}

impl Field {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "title" => Self::Title,
            "url" => Self::Url,
            "content" => Self::Content,
            "guid" => Self::Guid,
            "authors" => Self::Authors,
            "categories" => Self::Categories,
            _ => return None,
        })
    }

    const ALL: [Self; 6] = [
        Self::Title,
        Self::Url,
        Self::Content,
        Self::Guid,
        Self::Authors,
        Self::Categories,
    ];

    /// The field's values in `entry`.
    fn values(self, entry: &Entry) -> &[String] {
        match self {
            Self::Title => std::slice::from_ref(&entry.title),
            Self::Url => entry.url.as_slice(),
            Self::Content => entry.content.as_slice(),
            Self::Guid => std::slice::from_ref(&entry.guid),
            Self::Authors => &entry.authors,
            Self::Categories => &entry.categories,
        }
    }

    /// Whether `re` matches the field of `entry`, or one of its values.
    fn matches(self, re: &Regex, entry: &Entry) -> bool {
        match self {
            Self::Title => re.is_match(&entry.title),
            Self::Url => entry.url.as_deref().is_some_and(|s| re.is_match(s)),
            Self::Content => entry.content.as_deref().is_some_and(|s| re.is_match(s)),
            Self::Guid => re.is_match(&entry.guid),
            Self::Authors => entry.authors.iter().any(|s| re.is_match(s)),
            Self::Categories => entry.categories.iter().any(|s| re.is_match(s)),
        }
    }
}

const DEFAULT_FIELDS: &[Field] = &[Field::Title, Field::Content];

struct Rule {
    re: Regex,
    /// The pattern and its flags, for [`FieldSet`].
    source: (String, String),
    fields: Vec<Field>,
    /// The feeds the rule applies to, by id, or `None` for every feed.
    feeds: Option<HashSet<i64>>,
    /// The feeds the rule applies to, by URL; empty if it names none that way.
    urls: HashSet<String>,
    /// Where the rule is in the config, such as `exclude[1]`.
    place: String,
}

impl Rule {
    fn compile(place: String, rule: &Value) -> Result<Self, String> {
        let fail = |message: &str| Err(format!("filter: {place}: {message}"));
        let Some(rule) = rule.as_object() else {
            return fail("a rule must be a table");
        };
        let Some(pattern) = rule.get("pattern").and_then(Value::as_str) else {
            return fail("'pattern' must be a string");
        };

        let names: Vec<&Value> = match rule.get("fields") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(names)) => names.iter().collect(),
            Some(Value::Object(names)) if names.is_empty() => Vec::new(),
            Some(name @ Value::String(_)) => vec![name],
            Some(_) => return fail("'fields' must be a field name or a list of them"),
        };
        let fields = if names.is_empty() {
            DEFAULT_FIELDS.to_vec()
        } else {
            let mut fields = Vec::with_capacity(names.len());
            for name in names {
                match name.as_str().and_then(Field::parse) {
                    Some(field) => fields.push(field),
                    None => {
                        let shown = match name {
                            Value::String(s) => format!("{s:?}"),
                            other => format!("\"{other}\""),
                        };
                        return fail(&format!(
                            "unknown field {shown}; expected title, url, content, authors, \
                             categories or guid"
                        ));
                    }
                }
            }
            fields
        };

        let flags = match rule.get("flags") {
            None | Some(Value::Null) => "",
            Some(Value::String(flags)) => flags.as_str(),
            Some(_) => return fail("'flags' must be a string"),
        };
        // The server compiles patterns as `kiki.regex` does, so a pattern the Lua plugin
        // accepts is accepted here too, and no other.
        let re = match Regex::compile(pattern, flags) {
            Ok(re) => re,
            Err(e) => return fail(&format!("kiki.regex: {e}")),
        };

        let (mut feeds, mut urls) = (None, HashSet::new());
        match rule.get("feeds") {
            None | Some(Value::Null) => {}
            Some(Value::Array(list)) => {
                let mut ids = HashSet::new();
                for feed in list {
                    match feed {
                        Value::String(url) => {
                            urls.insert(url.clone());
                        }
                        other => match as_integer(other) {
                            Some(id) => {
                                ids.insert(id);
                            }
                            None => return fail("'feeds' must be a list of feed ids or URLs"),
                        },
                    }
                }
                feeds = Some(ids);
            }
            Some(Value::Object(map)) if map.is_empty() => feeds = Some(HashSet::new()),
            Some(_) => return fail("'feeds' must be a list of feed ids or URLs"),
        }

        Ok(Rule {
            re,
            source: (pattern.to_string(), flags.to_string()),
            fields,
            feeds,
            urls,
            place,
        })
    }

    fn matches(&self, entry: &Entry) -> bool {
        self.fields.iter().any(|f| f.matches(&self.re, entry))
    }
}

/// `n` as an integer, if it is one, as Lua's `math.tointeger` reads it.
fn as_integer(n: &Value) -> Option<i64> {
    let n = n.as_number()?;
    n.as_i64().or_else(|| {
        let f = n.as_f64()?;
        // `i64::MAX as f64` rounds up to 2^63, which is out of range.
        (f.fract() == 0.0 && f >= i64::MIN as f64 && f < i64::MAX as f64).then_some(f as i64)
    })
}

fn compile_rules(config: &Map<String, Value>, name: &str) -> Result<Vec<Rule>, String> {
    let list = match config.get(name) {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(list)) => list,
        Some(Value::Object(map)) if map.is_empty() => return Ok(Vec::new()),
        Some(_) => return Err(format!("filter: {name}: must be a list of rules")),
    };
    list.iter()
        .enumerate()
        .map(|(i, rule)| Rule::compile(format!("{name}[{}]", i + 1), rule))
        .collect()
}

/// Whether `a` and `b` are equal as the Lua plugin compares them: an empty list and an
/// empty object are both an empty table, and numbers are equal if their values are.
fn same(a: &Value, b: &Value) -> bool {
    let empty = |v: &Value| match v {
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    };
    match (a, b) {
        _ if empty(a) && empty(b) => true,
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            // Lua has no nulls: a key holding one is absent.
            let present = |o: &Map<String, Value>| o.values().filter(|v| !v.is_null()).count();
            present(x) == present(y)
                && x.iter()
                    .filter(|(_, v)| !v.is_null())
                    .all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

/// The `exclude` rules that read a field, matched together, so that each of the field's
/// values goes to the server once for all of them rather than once per rule. The server
/// shares the rules' compiled patterns between the sets and the rules.
struct FieldSet {
    field: Field,
    set: RegexSet,
    /// The index in `exclude` of the rule each of the set's patterns comes from.
    rules: Vec<usize>,
}

impl FieldSet {
    /// One set for each field the rules in `exclude` read.
    fn build(exclude: &[Rule]) -> Result<Vec<Self>, String> {
        let mut sets = Vec::new();
        for field in Field::ALL {
            let rules: Vec<usize> = (0..exclude.len())
                .filter(|&i| exclude[i].fields.contains(&field))
                .collect();
            if rules.is_empty() {
                continue;
            }
            let sources: Vec<(String, String)> =
                rules.iter().map(|&i| exclude[i].source.clone()).collect();
            let set = RegexSet::compile(&sources).map_err(|e| format!("filter: exclude: {e}"))?;
            sets.push(FieldSet { field, set, rules });
        }
        Ok(sets)
    }
}

struct Filter {
    exclude: Vec<Rule>,
    exclude_sets: Vec<FieldSet>,
    include: Vec<Rule>,
    /// Whether to apply changed rules to the entries already stored.
    rescan: bool,
    /// The rules, as the config gives them, to record once they have been applied.
    rules: Value,
    /// Whether any rule names feeds by URL, so that feeds need looking up.
    by_url: bool,
    feed_urls: FeedUrls,
    /// The scan applying the rules to the stored entries, once one has started.
    scan: Option<u64>,
}

/// The URL of each feed looked up so far, by feed id; `None` for a feed with no URL, or
/// that does not exist.
#[derive(Default)]
struct FeedUrls(HashMap<i64, Option<String>>);

impl FeedUrls {
    /// The URL of the feed `feed_id`, looked up once.
    fn get(&mut self, feed_id: i64) -> Option<&str> {
        if let hash_map::Entry::Vacant(slot) = self.0.entry(feed_id) {
            match host::get_feed(feed_id) {
                Ok(feed) => {
                    slot.insert(feed.and_then(|feed| feed.url));
                }
                Err(e) => {
                    // Not remembered, so that the next entry tries again.
                    log(
                        Level::Warn,
                        &format!("filter: looking up feed {feed_id}: {e}"),
                    );
                    return None;
                }
            }
        }
        self.0.get(&feed_id)?.as_deref()
    }
}

impl Rule {
    /// Whether the rule applies to the entries of the feed `feed_id`.
    fn applies(&self, feed_id: i64, feed_urls: &mut FeedUrls) -> bool {
        match &self.feeds {
            None => true,
            Some(ids) if ids.contains(&feed_id) => true,
            Some(_) if self.urls.is_empty() => false,
            Some(_) => feed_urls
                .get(feed_id)
                .is_some_and(|url| self.urls.contains(url)),
        }
    }
}

impl Filter {
    /// Why `entry` should be hidden, or `None` if it should not be.
    fn reason_to_hide(&mut self, entry: &Entry) -> Option<String> {
        let feed_urls = &mut self.feed_urls;
        if !self.exclude_sets.is_empty() {
            let mut matched = vec![false; self.exclude.len()];
            for set in &self.exclude_sets {
                for value in set.field.values(entry) {
                    for i in set.set.matches(value) {
                        if let Some(&rule) = set.rules.get(i as usize) {
                            matched[rule] = true;
                        }
                    }
                }
            }
            // The first rule that matched, as the rules are tried in order.
            if let Some(rule) = self
                .exclude
                .iter()
                .zip(matched)
                .find(|(r, matched)| *matched && r.applies(entry.feed_id, feed_urls))
            {
                return Some(format!("{} matched", rule.0.place));
            }
        }
        let mut any_include = false;
        for rule in &self.include {
            if rule.applies(entry.feed_id, feed_urls) {
                if rule.matches(entry) {
                    return None;
                }
                any_include = true;
            }
        }
        any_include.then(|| "no include rule matched".to_string())
    }

    /// Tags `entry` hidden if it should be; whether that added the tag.
    fn filter(&mut self, entry: &mut Entry) -> bool {
        if self.exclude.is_empty() && self.include.is_empty() {
            return false;
        }
        let Some(reason) = self.reason_to_hide(entry) else {
            return false;
        };
        log(
            Level::Debug,
            &format!("filter: hiding {:?}: {reason}", entry.guid),
        );
        if entry.tags.iter().any(|t| t == HIDDEN) {
            return false;
        }
        entry.tags.push(HIDDEN.to_string());
        true
    }
}

impl Plugin for Filter {
    fn new(config: &str) -> Result<Self, String> {
        let config: Map<String, Value> =
            serde_json::from_str(config).map_err(|e| format!("filter: invalid config: {e}"))?;
        // Compiled here, so that a bad rule fails when the plugin loads rather than on
        // every entry.
        let exclude = compile_rules(&config, "exclude")?;
        let include = compile_rules(&config, "include")?;

        if config
            .get("tag")
            .and_then(Value::as_array)
            .is_some_and(|tag| !tag.is_empty())
        {
            log(
                Level::Warn,
                "filter: ignoring the 'tag' rules; the filter no longer tags entries, so \
                 move them to the auto-tag plugin's rules",
            );
        }

        let rule_list = |name| match config.get(name) {
            None | Some(Value::Null) => Value::Array(Vec::new()),
            Some(list) => list.clone(),
        };
        let rules = serde_json::json!({
            "exclude": rule_list("exclude"),
            "include": rule_list("include"),
        });
        let by_url = exclude.iter().chain(&include).any(|r| !r.urls.is_empty());
        let exclude_sets = FieldSet::build(&exclude)?;
        Ok(Filter {
            exclude,
            exclude_sets,
            include,
            rescan: config.get("rescan") != Some(&Value::Bool(false)),
            rules,
            by_url,
            feed_urls: FeedUrls::default(),
            scan: None,
        })
    }
}

#[plugin]
impl Filter {
    #[on(entry.ingest)]
    fn filter_ingested(&mut self, mut entry: Entry) -> Option<Entry> {
        self.filter(&mut entry);
        Some(entry)
    }

    // A feed's id may be given to a new feed once it is removed.
    #[on(feed.removed)]
    fn forget_feed(&mut self, feed: FeedEvent) {
        if self.by_url {
            self.feed_urls.0.remove(&feed.id);
        }
    }

    // When the rules change, apply them to the entries already stored. The rules last
    // applied are kept in the plugin's store, so that restarting the server, or reloading
    // plugins for some other reason, does not rescan: that would hide again any entry the
    // user unhid.
    //
    // The rules are only recorded as applied once the scan has gone through every entry.
    // A scan cut short, by a reload or the server stopping, runs again from the start on
    // the next load.
    #[on(plugin.load)]
    fn apply_changed_rules(&mut self) {
        if !self.rescan {
            return;
        }
        let mut recorded = match host::store_get(RULES_KEY) {
            Ok(recorded) => recorded.and_then(|text| serde_json::from_str(&text).ok()),
            Err(e) => {
                log(
                    Level::Warn,
                    &format!("filter: reading the rules applied: {e}"),
                );
                return;
            }
        };
        // Versions before 3.0.0 recorded their tag rules too. Those no longer apply, so
        // dropping them should not rescan.
        if let Some(Value::Object(recorded)) = &mut recorded {
            recorded.remove("tag");
        }
        if recorded.as_ref().is_some_and(|r| same(r, &self.rules)) {
            return;
        }
        if self.exclude.is_empty() && self.include.is_empty() {
            self.record_rules();
            return;
        }
        log(
            Level::Info,
            "filter: rules changed; applying them to stored entries",
        );
        match host::start_scan(ScanOptions {
            feed_id: None,
            since: None,
            include_hidden: false,
        }) {
            Ok(scan) => self.scan = Some(scan),
            Err(e) => log(Level::Warn, &format!("filter: starting a scan: {e}")),
        }
    }

    #[on(scan.entry)]
    fn filter_stored(&mut self, scan: u64, mut entry: Entry) -> Option<Entry> {
        // Entries left as they are need not be sent back.
        (self.scan == Some(scan) && self.filter(&mut entry)).then_some(entry)
    }

    #[on(scan.done)]
    fn finish_scan(&mut self, scan: u64, summary: kiki_plugin::ScanSummary) {
        if self.scan != Some(scan) {
            return;
        }
        self.scan = None;
        log(
            Level::Info,
            &format!(
                "filter: applied the rules to {} stored entries, hiding {}",
                summary.scanned, summary.updated
            ),
        );
        self.record_rules();
    }
}

impl Filter {
    fn record_rules(&self) {
        if let Err(e) = host::set(RULES_KEY, &self.rules) {
            log(
                Level::Warn,
                &format!("filter: recording the rules applied: {e}"),
            );
        }
    }
}
