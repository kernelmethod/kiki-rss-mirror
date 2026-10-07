//! An example Kiki plugin in Rust: hide entries whose title or content matches a
//! regular expression. See the guide's "Writing WebAssembly plugins" chapter.

use kiki_plugin::regex::Regex;
use kiki_plugin::{export_plugin, parse_config, Entry, EventKind, Plugin};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    rules: Vec<RuleConfig>,
}

#[derive(Deserialize)]
struct RuleConfig {
    pattern: String,
    #[serde(default)]
    field: Field,
}

#[derive(Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Field {
    #[default]
    Title,
    Content,
}

struct HideMatching {
    rules: Vec<(Field, Regex)>,
}

impl Plugin for HideMatching {
    const EVENTS: &'static [EventKind] = &[EventKind::EntryIngest];

    fn new(config: &str) -> Result<Self, String> {
        let config: Config = parse_config(config)?;
        let rules = config
            .rules
            .into_iter()
            .map(|rule| {
                // A bad pattern fails the load, rather than every entry.
                let re = Regex::compile(&rule.pattern, "")
                    .map_err(|e| format!("bad pattern {:?}: {e}", rule.pattern))?;
                Ok((rule.field, re))
            })
            .collect::<Result<_, String>>()?;
        Ok(HideMatching { rules })
    }

    fn on_entry_ingest(&mut self, mut entry: Entry) -> Option<Entry> {
        let hide = self.rules.iter().any(|(field, re)| match field {
            Field::Title => re.is_match(&entry.title),
            Field::Content => entry.content.as_deref().is_some_and(|c| re.is_match(c)),
        });
        if hide {
            entry.tags.push("system:hidden".to_string());
        }
        Some(entry)
    }
}

export_plugin!(HideMatching);
