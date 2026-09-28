PRAGMA foreign_keys = ON;

-- Enable incremental auto-vacuum so freed pages can be reclaimed by
-- `PRAGMA incremental_vacuum` without a full database rebuild. Must be set
-- before any tables are created for it to persist in the database file.
PRAGMA auto_vacuum = INCREMENTAL;

CREATE TABLE migrations (
    id          INTEGER PRIMARY KEY,
    name        VARCHAR NOT NULL UNIQUE,
    applied_at  DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Persistent record of when recurring background tasks last ran, so their
-- schedules survive server restarts. Keyed by an opaque task name.
CREATE TABLE task_queue (
    task_type    TEXT PRIMARY KEY NOT NULL,
    last_run_at  INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Tags come in two kinds:
--   * 'user'   — created and managed by the user.
--   * 'system' — built-in tags recording entry metadata (read, saved,
--                hidden). They are seeded below, cannot be renamed or
--                deleted, and their names all start with the reserved
--                prefix 'system:', which user tags may not use.
CREATE TABLE tags (
    id      INTEGER PRIMARY KEY,
    name    VARCHAR UNIQUE NOT NULL,
    kind    VARCHAR NOT NULL DEFAULT 'user' CHECK (kind IN ('user', 'system'))
);

-- Keep in sync with `SystemTag` in src/db/tags.rs.
INSERT INTO tags (name, kind) VALUES
    ('system:read', 'system'),
    ('system:saved', 'system'),
    ('system:hidden', 'system');

CREATE TABLE scripts (
    id      INTEGER PRIMARY KEY,
    engine  VARCHAR NOT NULL,
    text    VARCHAR NOT NULL,
    kind    VARCHAR NOT NULL
);
CREATE INDEX idx_scripts_kind ON scripts(kind);

---------------------------------------------------------------------------------
-- Tables for feeds
---------------------------------------------------------------------------------

CREATE TABLE feeds (
    id                      INTEGER PRIMARY KEY,

    -- "rss" or "atom"
    -- Should be NULL if unknown (e.g. if the feed was just created)
    syndication_format      VARCHAR,

    title                   VARCHAR NOT NULL,
    url                     VARCHAR,
    description             VARCHAR,
    last_checked            DATETIME,
    header_etag             VARCHAR,
    header_last_modified    VARCHAR,

    -- Blake3 hex digest of the body returned by the most recent successful
    -- 200 response. Paired with `last_full_refresh_at` to detect servers
    -- that keep returning unchanged `ETag`/`Last-Modified` validators while
    -- the body has actually changed.
    header_body_hash        VARCHAR,

    -- Unix timestamp of the most recent successful 200 response (forced or
    -- conditional). Used to decide when to force another non-conditional
    -- fetch for validator-lie detection.
    last_full_refresh_at    INTEGER,

    -- Unix timestamp parsed from the HTTP Expires response header.
    -- When set, the fetcher will skip refreshing the feed until this
    -- time has passed.
    header_expires          INTEGER,

    -- Unix timestamp until which the feed's last response is treated as
    -- immutable per RFC 8246 (Cache-Control: immutable). While set and in
    -- the future, conditional request headers (If-None-Match,
    -- If-Modified-Since) are omitted and the feed is not refetched.
    header_immutable_until  INTEGER,

    -- Refresh hints the feed declares in its own markup, captured from the
    -- most recent successfully parsed 200 response:
    --   * feed_ttl_seconds: RSS <ttl>, converted from minutes.
    --   * feed_update_interval_seconds: sy:updatePeriod / sy:updateFrequency.
    --   * feed_skip_hours: RSS <skipHours> as a bitmask (bit h = hour h UTC).
    --   * feed_skip_days: RSS <skipDays> as a bitmask (bit 0 = Monday).
    -- The longer of the two intervals is a fallback freshness hint when the
    -- HTTP response carries none; the skip masks defer every scheduled
    -- fetch out of the hours and days the publisher asked us to avoid.
    feed_ttl_seconds                INTEGER,
    feed_update_interval_seconds    INTEGER,
    feed_skip_hours                 INTEGER NOT NULL DEFAULT 0,
    feed_skip_days                  INTEGER NOT NULL DEFAULT 0,

    -- Most recent fetch error message, if any. Cleared on successful fetch.
    last_fetch_error        VARCHAR,
    -- Timestamp of the most recent fetch error.
    last_fetch_error_at     DATETIME,

    -- Minimum interval, in seconds, between fetches of this feed. Acts as
    -- a ceiling on polling interval: even if the server advertises a
    -- longer max-age, we will refresh at least this often. Also used as
    -- the fallback interval when the server sends no cache hint.
    -- Defaults to 3 hours (10800 seconds).
    min_fetch_interval_seconds  INTEGER NOT NULL DEFAULT 10800,

    -- Unix timestamp (seconds) of the earliest moment this feed is
    -- eligible for the next fetch. NULL means "fetch immediately" and is
    -- the default for newly created feeds. Updated after every fetch
    -- attempt (success, 304, or error) using server cache hints,
    -- Retry-After, or exponential backoff.
    next_fetch_at           INTEGER,

    -- Number of consecutive transient failures. Reset to 0 on any
    -- successful fetch (including 304). Drives exponential backoff.
    consecutive_failures    INTEGER NOT NULL DEFAULT 0,

    -- Unix timestamp (seconds) parsed from the most recent `Retry-After`
    -- response header, if any. Retained for observability; the scheduled
    -- retry time is folded into `next_fetch_at`.
    retry_after_at          INTEGER,

    -- Per-feed authentication. `auth_type` is one of:
    --   * NULL or 'none' — no authentication (default)
    --   * 'basic'        — HTTP Basic auth (auth_username + auth_password)
    --   * 'bearer'       — HTTP Bearer token (auth_bearer_token)
    --
    -- Credentials are stored in plaintext; restrict filesystem access to
    -- the database file and treat it as sensitive.
    auth_type               VARCHAR,
    auth_username           VARCHAR,
    auth_password           VARCHAR,
    auth_bearer_token       VARCHAR
);

CREATE INDEX idx_feeds_next_fetch_at ON feeds(next_fetch_at);

-- A list of the tags that are automatically assigned to entries from a given
-- feed
CREATE TABLE feed_tags (
    feed_id INTEGER NOT NULL,
    tag_id  INTEGER NOT NULL,
    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE,
    FOREIGN KEY(tag_id) REFERENCES tags(id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX idx_feed_tags_unique ON feed_tags(feed_id, tag_id);

-- A list of scripts that should run on entries retrieved for a given feed
CREATE TABLE feed_scripts (
    feed_id INTEGER NOT NULL,
    script_id INTEGER NOT NULL,

    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE,
    FOREIGN KEY(script_id) REFERENCES scripts(id) ON DELETE CASCADE
);

---------------------------------------------------------------------------------
-- Tables for entry sources
--
-- Sources are RSS/Atom channels that content comes from. Entries in an RSS or
-- Atom feed can reference a source if they received their content from another
-- feed. This can be useful for creating feed aggregators.
---------------------------------------------------------------------------------

CREATE TABLE entry_sources (
    id                  INTEGER PRIMARY KEY,
    entry_id            INTEGER,

    FOREIGN KEY(entry_id) REFERENCES entries(id) ON DELETE CASCADE
);

---------------------------------------------------------------------------------
-- Tables for feed entries
--
-- These contain information common to entries from all syndication formats
---------------------------------------------------------------------------------

CREATE TABLE entries (
    id                  INTEGER PRIMARY KEY,
    feed_id             INTEGER,
    source_id           INTEGER,

    -- "rss" or "atom"
    syndication_format  VARCHAR NOT NULL,

    guid                VARCHAR NOT NULL,

    published_at    DATETIME NOT NULL,
    title           VARCHAR NOT NULL,
    url             VARCHAR NOT NULL,
    content         VARCHAR,

    -- Unix timestamp of the first successful refresh of the entry's feed
    -- that no longer listed it. NULL while the feed still carries the
    -- entry; cleared if it reappears. Retention only deletes entries that
    -- have been dropped for longer than `retention.max_age_days`.
    dropped_at      INTEGER,
    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE SET NULL,
    FOREIGN KEY(source_id) REFERENCES entry_sources(id) ON DELETE SET NULL
);
CREATE UNIQUE INDEX idx_entry_guids ON entries(feed_id, guid);
CREATE INDEX idx_entry_dropped_at ON entries(dropped_at) WHERE dropped_at IS NOT NULL;

-- Table mapping entries to the tags that they belong to
CREATE TABLE entry_tags (
    entry_id    INTEGER NOT NULL,
    tag_id      INTEGER NOT NULL,

    FOREIGN KEY(entry_id) REFERENCES entries(id) ON DELETE CASCADE,
    FOREIGN KEY(tag_id) REFERENCES tags(id) ON DELETE CASCADE
);
CREATE INDEX idx_entry_published_at ON entries(published_at);
-- Serves per-feed entry listings in newest-first order. SQLite appends the
-- rowid (entries.id) to every index, so this also covers the id tie-breaker.
CREATE INDEX idx_entry_feed_published_at ON entries(feed_id, published_at);
-- Also serves lookups by entry_id.
CREATE UNIQUE INDEX idx_entry_tags_unique ON entry_tags(entry_id, tag_id);
CREATE INDEX idx_entry_tags_tag_id ON entry_tags(tag_id);

---------------------------------------------------------------------------------
-- RSS-related data
--
-- https://www.rssboard.org/rss-specification
---------------------------------------------------------------------------------
-- RSS-specific per-entry data. `entry_id` is the PK; there's a 1:1
-- relationship with `entries`.
CREATE TABLE rss_entry_data (
    entry_id        INTEGER PRIMARY KEY,

    -- The item's <description>, which is also stored as `entries.content`.
    -- Only set when a script changed the content; NULL means "same as
    -- entries.content" so the text is not stored twice.
    description     VARCHAR,

    comments        VARCHAR,
    author          VARCHAR,

    -- Enclosure information
    enclosure_url       VARCHAR,
    enclosure_length    INTEGER,
    enclosure_mime_type VARCHAR,

    FOREIGN KEY(entry_id) REFERENCES entries(id) ON DELETE CASCADE
);

-- A single RSS entry may carry multiple <category> elements. Rows are
-- uniquely keyed by (entry_id, category, domain) via the index below;
-- SQLite's implicit rowid orders them by insertion for display.
CREATE TABLE rss_categories (
    entry_id        INTEGER NOT NULL,
    category        VARCHAR NOT NULL,
    domain          VARCHAR,

    FOREIGN KEY(entry_id) REFERENCES entries(id) ON DELETE CASCADE
);
-- The composite unique index below has `entry_id` as its leftmost column,
-- so it also serves `WHERE entry_id = ?` lookups and cascade-deletes — no
-- separate single-column index is needed.
CREATE UNIQUE INDEX idx_rss_categories_unique
    ON rss_categories(entry_id, category, COALESCE(domain, ''));

---------------------------------------------------------------------------------
-- Atom-related tables
--
-- https://www.rfc-editor.org/rfc/rfc4287
---------------------------------------------------------------------------------

-- Atom-specific feed data. Holds feed-level fields (atom_uri,
-- atom_language_tag); the atom_feed_* child tables FK directly to
-- feeds(id), so this row is no longer a join-point.
CREATE TABLE atom_feed_data (
    id      INTEGER PRIMARY KEY,
    feed_id INTEGER NOT NULL,

    -- atomCommonAttributes
    atom_uri            VARCHAR,
    atom_language_tag   VARCHAR,

    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX idx_atom_feed_data ON atom_feed_data(feed_id);

-- Atom-specific source data
CREATE TABLE atom_source_data (
    id          INTEGER PRIMARY KEY,
    source_id   INTEGER,

    FOREIGN KEY(source_id) REFERENCES entry_sources(id) ON DELETE CASCADE
);
CREATE INDEX idx_atom_source_data ON atom_source_data(source_id);

-- Data for atom:rights elements
CREATE TABLE atom_feed_rights (
    feed_id INTEGER PRIMARY KEY,
    rights  VARCHAR,

    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE
);

CREATE TABLE atom_entry_rights (
    entry_id INTEGER PRIMARY KEY,
    rights   VARCHAR,

    FOREIGN KEY(entry_id) REFERENCES entries(id) ON DELETE CASCADE
);

-- Data for atom:generator elements
CREATE TABLE atom_feed_generators (
    feed_id     INTEGER PRIMARY KEY,

    -- atomCommonAttributes
    atom_uri            VARCHAR,
    atom_language_tag   VARCHAR,

    -- atom:generator
    value               VARCHAR NOT NULL,
    uri                 VARCHAR,
    version             VARCHAR,

    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE
);

-- Data for atom:logo elements
CREATE TABLE atom_feed_logos (
    feed_id     INTEGER PRIMARY KEY,

    -- atomCommonAttributes
    atom_uri            VARCHAR,
    atom_language_tag   VARCHAR,

    -- atom:logo
    uri                 VARCHAR NOT NULL,

    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE
);

-- Data for atom:icon elements
CREATE TABLE atom_feed_icons (
    feed_id     INTEGER PRIMARY KEY,

    -- atomCommonAttributes
    atom_uri            VARCHAR,
    atom_language_tag   VARCHAR,

    -- atom:icon
    uri                 VARCHAR NOT NULL,

    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE
);

-- Data for atom:category elements
CREATE TABLE atom_categories (
    id  INTEGER PRIMARY KEY,

    -- atomCommonAttributes
    atom_uri            VARCHAR,
    atom_language_tag   VARCHAR,

    -- atom:category
    category    VARCHAR NOT NULL,
    scheme      VARCHAR,
    label       VARCHAR,

    undefined_content   VARCHAR
);

CREATE TABLE atom_feed_categories (
    feed_id     INTEGER,
    category_id INTEGER,

    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE,
    FOREIGN KEY(category_id) REFERENCES atom_categories(id) ON DELETE CASCADE
);
CREATE INDEX idx_atom_feed_categories ON atom_feed_categories(feed_id);
CREATE INDEX idx_atom_feed_categories_category
    ON atom_feed_categories(category_id);

CREATE TABLE atom_entry_categories (
    entry_id    INTEGER,
    category_id INTEGER,

    FOREIGN KEY(entry_id) REFERENCES entries(id) ON DELETE CASCADE,
    FOREIGN KEY(category_id) REFERENCES atom_categories(id) ON DELETE CASCADE
);
CREATE INDEX idx_atom_entry_categories ON atom_entry_categories(entry_id);
CREATE INDEX idx_atom_entry_categories_category
    ON atom_entry_categories(category_id);

-- Data for atom:author elements
CREATE TABLE atom_feed_authors (
    id      INTEGER PRIMARY KEY,
    feed_id INTEGER NOT NULL,
    author  VARCHAR NOT NULL,

    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE
);
CREATE INDEX idx_atom_feed_authors_feed ON atom_feed_authors(feed_id);

CREATE TABLE atom_entry_authors (
    id       INTEGER PRIMARY KEY,
    entry_id INTEGER NOT NULL,
    author   VARCHAR NOT NULL,

    FOREIGN KEY(entry_id) REFERENCES entries(id) ON DELETE CASCADE
);
CREATE INDEX idx_atom_entry_authors_entry ON atom_entry_authors(entry_id);

-- Data for atom:contributor elements
CREATE TABLE atom_feed_contributors (
    id          INTEGER PRIMARY KEY,
    feed_id     INTEGER NOT NULL,
    contributor VARCHAR NOT NULL,

    FOREIGN KEY(feed_id) REFERENCES feeds(id) ON DELETE CASCADE
);
CREATE INDEX idx_atom_feed_contributors_feed ON atom_feed_contributors(feed_id);

CREATE TABLE atom_entry_contributors (
    id          INTEGER PRIMARY KEY,
    entry_id    INTEGER NOT NULL,
    contributor VARCHAR NOT NULL,

    FOREIGN KEY(entry_id) REFERENCES entries(id) ON DELETE CASCADE
);
CREATE INDEX idx_atom_entry_contributors_entry ON atom_entry_contributors(entry_id);

-- Unions per-entry contributors with per-feed contributors expanded onto
-- every entry of the feed.
CREATE VIEW atom_entry_contributors_all AS
SELECT entry_id, contributor FROM atom_entry_contributors
UNION
SELECT e.id AS entry_id, afc.contributor AS contributor
FROM entries e
JOIN atom_feed_contributors afc ON e.feed_id = afc.feed_id;

---------------------------------------------------------------------------------
-- Cached feed assets (images, enclosures) fetched from entry content.
---------------------------------------------------------------------------------

CREATE TABLE feed_assets (
    id               INTEGER PRIMARY KEY,
    blake3           TEXT NOT NULL UNIQUE,
    original_url     TEXT NOT NULL,
    content_type     TEXT,
    size_bytes       INTEGER NOT NULL,
    cached_at        INTEGER NOT NULL DEFAULT (unixepoch()),
    last_accessed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    etag             TEXT,
    last_modified    TEXT
);
CREATE INDEX idx_feed_assets_last_accessed ON feed_assets(last_accessed_at);
CREATE INDEX idx_feed_assets_original_url  ON feed_assets(original_url);

CREATE TABLE entry_assets (
    entry_id INTEGER NOT NULL,
    asset_id INTEGER NOT NULL,
    kind     TEXT NOT NULL,
    PRIMARY KEY (entry_id, asset_id),
    FOREIGN KEY (entry_id) REFERENCES entries(id) ON DELETE CASCADE,
    FOREIGN KEY (asset_id) REFERENCES feed_assets(id) ON DELETE CASCADE
);
CREATE INDEX idx_entry_assets_asset ON entry_assets(asset_id);

---------------------------------------------------------------------------------
-- Full-text search index over entries (FTS5, external content)
---------------------------------------------------------------------------------

CREATE VIRTUAL TABLE entries_fts USING fts5(
    title,
    content,
    url,
    content=entries,
    content_rowid=id
);

-- Keep the FTS index in sync with the entries table.
CREATE TRIGGER entries_fts_ai AFTER INSERT ON entries BEGIN
    INSERT INTO entries_fts(rowid, title, content, url)
    VALUES (new.id, new.title, new.content, new.url);
END;

CREATE TRIGGER entries_fts_bd BEFORE DELETE ON entries BEGIN
    INSERT INTO entries_fts(entries_fts, rowid, title, content, url)
    VALUES ('delete', old.id, old.title, old.content, old.url);
END;

-- Only the indexed columns: marking entries dropped touches many rows at
-- once and should not reindex them.
CREATE TRIGGER entries_fts_au AFTER UPDATE OF title, content, url ON entries BEGIN
    INSERT INTO entries_fts(entries_fts, rowid, title, content, url)
    VALUES ('delete', old.id, old.title, old.content, old.url);
    INSERT INTO entries_fts(rowid, title, content, url)
    VALUES (new.id, new.title, new.content, new.url);
END;

---------------------------------------------------------------------------------
-- Additional views to make it easy to retrieve feed and entry information
-- in a format-independent way.
---------------------------------------------------------------------------------
