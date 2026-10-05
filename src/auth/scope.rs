//! What an API token may do: [`Scope`]s, and sets of them, [`Scopes`].

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// One kind of access an API token can be granted.
///
/// Some scopes include others: [`Scope::Tags`] includes [`Scope::State`],
/// and [`Scope::Admin`] includes every scope. A [`Scopes`] set always holds
/// the scopes its members include, so checking for one is a single test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Read feeds, entries, tags and cached assets, search, and export
    /// OPML.
    Read,
    /// Mark entries read, saved or hidden.
    State,
    /// Create, rename and delete tags, and tag entries and feeds.
    Tags,
    /// Add, change, refresh and delete feeds, import OPML, and delete
    /// entries.
    Feeds,
    /// Scrape the Prometheus metrics at `/metrics`.
    Metrics,
    /// Everything: settings, plugins, tokens and shutting the server down,
    /// as well as every other scope.
    Admin,
}

impl Scope {
    /// Every scope, in the order they are listed.
    pub const ALL: [Scope; 6] = [
        Scope::Read,
        Scope::State,
        Scope::Tags,
        Scope::Feeds,
        Scope::Metrics,
        Scope::Admin,
    ];

    /// The scope's name, as tokens and the API spell it.
    pub fn name(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::State => "state",
            Scope::Tags => "tags",
            Scope::Feeds => "feeds",
            Scope::Metrics => "metrics",
            Scope::Admin => "admin",
        }
    }

    fn bit(self) -> u32 {
        match self {
            Scope::Read => 1,
            Scope::State => 1 << 1,
            Scope::Tags => 1 << 2,
            Scope::Feeds => 1 << 3,
            Scope::Metrics => 1 << 4,
            Scope::Admin => 1 << 5,
        }
    }

    /// This scope and every scope it includes.
    fn closure(self) -> u32 {
        match self {
            Scope::Tags => Scope::Tags.bit() | Scope::State.bit(),
            Scope::Admin => Scope::ALL.iter().fold(0, |bits, s| bits | s.bit()),
            s => s.bit(),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An error parsing a [`Scope`] or [`Scopes`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown scope or preset {0:?}; scopes are read, state, tags, feeds, metrics and admin, \
     and presets are reader, curator and manager"
)]
pub struct UnknownScope(pub String);

impl FromStr for Scope {
    type Err = UnknownScope;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Scope::ALL
            .into_iter()
            .find(|scope| scope.name() == s)
            .ok_or_else(|| UnknownScope(s.to_owned()))
    }
}

/// A set of [`Scope`]s, closed under inclusion: a set holding
/// [`Scope::Tags`] also holds [`Scope::State`], and one holding
/// [`Scope::Admin`] holds every scope.
///
/// Sets are written as a comma-separated list of scope names, which may
/// also name a preset:
///
/// - `reader`: `read` and `state`
/// - `curator`: `read` and `tags` (and so `state`)
/// - `manager`: `read`, `tags` and `feeds`
///
/// # Examples
///
/// ```
/// use kiki_rss::auth::{Scope, Scopes};
///
/// let scopes: Scopes = "read,tags".parse()?;
/// assert!(scopes.contains(Scope::State));
/// assert!(!scopes.contains(Scope::Feeds));
/// assert_eq!(scopes.to_string(), "read,state,tags");
///
/// assert_eq!("curator".parse::<Scopes>()?, scopes);
/// assert!("admin".parse::<Scopes>()?.contains(Scope::Metrics));
/// # Ok::<(), kiki_rss::auth::UnknownScope>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Scopes(u32);

impl Scopes {
    /// No scopes at all.
    pub const NONE: Scopes = Scopes(0);

    /// Every scope; what [`Scope::Admin`] grants.
    pub fn all() -> Self {
        Scopes(Scope::Admin.closure())
    }

    /// The set holding `scope` and the scopes it includes.
    pub fn of(scope: Scope) -> Self {
        Scopes(scope.closure())
    }

    /// This set with `scope`, and the scopes it includes, added.
    #[must_use]
    pub fn with(self, scope: Scope) -> Self {
        Scopes(self.0 | scope.closure())
    }

    /// Whether the set grants `scope`.
    pub fn contains(self, scope: Scope) -> bool {
        self.0 & scope.bit() != 0
    }

    /// Whether the set holds no scopes.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every scope in `other` is also in this set.
    pub fn includes(self, other: Scopes) -> bool {
        self.0 & other.0 == other.0
    }

    /// The scopes in the set, in the order of [`Scope::ALL`].
    pub fn iter(self) -> impl Iterator<Item = Scope> {
        Scope::ALL.into_iter().filter(move |s| self.contains(*s))
    }

    /// The set as stored in the database.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// The set stored in the database as `bits`. Bits that name no scope
    /// are dropped, and the set is closed under inclusion.
    pub fn from_bits(bits: u32) -> Self {
        Scope::ALL
            .into_iter()
            .filter(|s| bits & s.bit() != 0)
            .fold(Scopes::NONE, Scopes::with)
    }

    /// The set a preset name stands for, if `name` is a preset.
    fn preset(name: &str) -> Option<Self> {
        let read = Scopes::of(Scope::Read);
        match name {
            "reader" => Some(read.with(Scope::State)),
            "curator" => Some(read.with(Scope::Tags)),
            "manager" => Some(read.with(Scope::Tags).with(Scope::Feeds)),
            _ => None,
        }
    }
}

impl FromIterator<Scope> for Scopes {
    fn from_iter<I: IntoIterator<Item = Scope>>(iter: I) -> Self {
        iter.into_iter().fold(Scopes::NONE, Scopes::with)
    }
}

impl fmt::Display for Scopes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.iter().map(Scope::name).collect();
        f.write_str(&names.join(","))
    }
}

impl FromStr for Scopes {
    type Err = UnknownScope;

    /// Parse a comma-separated list of scope and preset names. Whitespace
    /// around names is ignored, as are empty names.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .try_fold(Scopes::NONE, |scopes, name| match Scopes::preset(name) {
                Some(preset) => Ok(Scopes(scopes.0 | preset.0)),
                None => Ok(scopes.with(name.parse()?)),
            })
    }
}

/// Serialized as a list of scope names, such as `["read", "state"]`.
impl Serialize for Scopes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter().map(Scope::name))
    }
}

/// Deserialized from a list of scope and preset names.
impl<'de> Deserialize<'de> for Scopes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let names = Vec::<String>::deserialize(deserializer)?;
        names
            .iter()
            .map(|name| name.parse::<Scopes>())
            .try_fold(Scopes::NONE, |all, scopes| {
                Ok::<_, UnknownScope>(Scopes(all.0 | scopes?.0))
            })
            .map_err(serde::de::Error::custom)
    }
}

impl utoipa::PartialSchema for Scopes {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, SchemaType, Type};
        ArrayBuilder::new()
            .items(
                ObjectBuilder::new()
                    .schema_type(SchemaType::Type(Type::String))
                    .enum_values(Some(Scope::ALL.iter().map(|s| s.name()))),
            )
            .description(Some(
                "Scopes, by name. Requests may also name the presets reader, curator and manager.",
            ))
            .into()
    }
}

impl utoipa::ToSchema for Scopes {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inclusion_is_closed() {
        assert_eq!(Scopes::of(Scope::Tags).to_string(), "state,tags");
        assert_eq!(
            Scopes::all().to_string(),
            "read,state,tags,feeds,metrics,admin"
        );
        assert_eq!(Scopes::from_bits(Scope::Admin.bit()), Scopes::all());
        assert_eq!(Scopes::from_bits(1 << 31), Scopes::NONE);
    }

    #[test]
    fn parses_names_and_presets() {
        assert_eq!(
            " read , state ,".parse::<Scopes>(),
            "reader".parse::<Scopes>()
        );
        assert_eq!(
            "manager".parse::<Scopes>().map(|s| s.to_string()),
            Ok("read,state,tags,feeds".to_owned())
        );
        assert_eq!(
            "read,root".parse::<Scopes>(),
            Err(UnknownScope("root".to_owned()))
        );
        assert_eq!("".parse::<Scopes>(), Ok(Scopes::NONE));
    }

    #[test]
    fn serde_round_trip() -> serde_json::Result<()> {
        let scopes: Scopes = serde_json::from_str(r#"["curator", "metrics"]"#)?;
        assert_eq!(
            serde_json::to_string(&scopes)?,
            r#"["read","state","tags","metrics"]"#
        );
        assert!(serde_json::from_str::<Scopes>(r#"["nope"]"#).is_err());
        Ok(())
    }

    #[test]
    fn includes() {
        let manager: Scopes = "manager".parse().unwrap_or_default();
        assert!(manager.includes(Scopes::of(Scope::Tags)));
        assert!(!manager.includes(Scopes::of(Scope::Admin)));
        assert!(Scopes::all().includes(manager));
    }
}
