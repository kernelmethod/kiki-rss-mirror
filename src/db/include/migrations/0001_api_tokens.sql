-- API tokens, which grant access to the API with a set of scopes. See
-- src/auth/ for the token format; only a hash of each token's secret is
-- stored.
CREATE TABLE api_tokens (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            VARCHAR NOT NULL UNIQUE,
    secret_hash     BLOB NOT NULL,
    -- The token's scopes, as the bits of `Scopes` in src/auth/scope.rs.
    scopes          INTEGER NOT NULL,
    created_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    -- Unix timestamp after which the token is refused; NULL if it never
    -- expires.
    expires_at      INTEGER,
    -- Unix timestamp of the token's last use, updated at most once a
    -- minute.
    last_used_at    INTEGER
);
