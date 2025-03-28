CREATE TABLE schema_version (
    version VARCHAR NOT NULL
);

CREATE TABLE tags (
    id      INTEGER PRIMARY KEY,
    name    VARCHAR UNIQUE NOT NULL
);

CREATE TABLE scripts (
    id      INTEGER PRIMARY KEY,
    lang    VARCHAR NOT NULL,
    text    VARCHAR NOT NULL
);

CREATE TABLE feeds (
    id                      INTEGER PRIMARY KEY,
    title                   VARCHAR NOT NULL,
    feed_url                VARCHAR NOT NULL,
    last_checked            DATETIME DEFAULT NULL,
    header_etag             VARCHAR DEFAULT '',
    header_last_modified    VARCHAR DEFAULT ''
);

-- A list of the tags that are automatically assigned to entries from a given
-- feed
CREATE TABLE feed_tags (
    feed_id INTEGER NOT NULL,
    tag_id  INTEGER NOT NULL,
    FOREIGN KEY (feed_id) REFERENCES feeds(id) ON DELETE CASCADE,
    FOREIGN KEY (tag_id) REFERENCES tags(id) ON DELETE CASCADE
);

-- A list of scripts that should run on entries retrieved for a given feed
CREATE TABLE feed_scripts (
    feed_id INTEGER NOT NULL,
    script_id INTEGER NOT NULL,

    FOREIGN KEY (feed_id) REFERENCES feeds(id) ON DELETE CASCADE,
    FOREIGN KEY (script_id) REFERENCES scripts(id) ON DELETE CASCADE
);

CREATE TABLE entries (
    id              INTEGER PRIMARY KEY,
    feed_id         INTEGER NOT NULL,
    published_at    DATETIME NOT NULL,
    title           VARCHAR NOT NULL,
    url             VARCHAR NOT NULL,
    author          VARCHAR,
    content         VARCHAR,

    FOREIGN KEY (feed_id) REFERENCES feeds(id) ON DELETE CASCADE
);

-- Table mapping entries to the tags that they belong to
CREATE TABLE entry_tags (
    entry_id    INTEGER NOT NULL,
    tag_id      INTEGER NOT NULL,

    FOREIGN KEY (entry_id) REFERENCES entries(id) ON DELETE CASCADE,
    FOREIGN KEY (tag_id) REFERENCES tags(id) ON DELETE CASCADE
);
