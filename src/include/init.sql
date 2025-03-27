CREATE TABLE schema_version (
    version VARCHAR NOT NULL
);

CREATE TABLE tags (
    id      INTEGER PRIMARY KEY,
    name    VARCHAR UNIQUE NOT NULL
);

CREATE TABLE feeds (
    id                      INTEGER PRIMARY KEY,
    title                   VARCHAR NOT NULL,
    feed_url                VARCHAR NOT NULL,
    last_checked            DATETIME DEFAULT NULL,
    header_etag             VARCHAR DEFAULT '',
    header_last_modified    VARCHAR DEFAULT ''
);

-- A list of the tags that are automatically assigned to items
-- from a given feed
CREATE TABLE feed_tags (
    feed_id     INTEGER,
    tag_id      INTEGER
);
