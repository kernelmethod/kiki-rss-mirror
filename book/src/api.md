# The HTTP API

Everything Kiki does is available over a JSON HTTP API, served on Kiki's
Unix socket. The [interactive API reference](../api/) lists every endpoint
with its parameters, responses and the [token scope](tokens.md) it needs. A running
server also serves the same reference itself, at `/docs`.

With curl, pass the socket's path with `--unix-socket`; the host name in the
URL is ignored:

```bash
curl --unix-socket "$XDG_RUNTIME_DIR/kiki/kiki.sock" http://localhost/v1/feeds
```

## Concepts

- **Feeds** are the RSS or Atom feeds Kiki fetches, under `/v1/feeds`.
- **Entries** are the items in those feeds, under `/v1/entries`. Kiki keeps
  an entry after it drops out of its feed, until
  [`retention.max_age_days`](settings.md#retention) says otherwise.
- **Tags** label both feeds and entries, under `/v1/tags`. There are two
  kinds:
  - **User tags** are created, renamed, and deleted by you, by plugins, or
    by OPML imports.
  - **System tags** are built in and record common per-entry state:
    `system:read`, `system:saved`, and `system:hidden`. They cannot be
    renamed or deleted, and are applied to entries with `PUT` and `DELETE`
    on `/v1/entries/id/{id}/system-tags/{name}`. The `system:` prefix is
    reserved, so user tags cannot start with it.

  Entries tagged `system:hidden` are left out of entry lists unless asked
  for with `include_hidden=true`.

## Syncing entries

Clients that keep their own copy of Kiki's entries, such as a sync layer for
another reader's API, can fetch just what changed:

- **Entry IDs only ever increase**, even after entries are deleted, so the
  highest ID a client has seen marks where it left off. The entry listings
  (`GET /v1/entries`, `/v1/feeds/id/{id}/entries`,
  `/v1/tags/id/{id}/entries`) and `POST /v1/entries/search` take `since_id`
  and `max_id`, and sort by ID with `sort=id` or `sort=id_desc`. Passing the
  last ID of one page as the next page's `since_id` pages through entries
  without skipping any, unlike `offset`.
- **Every entry carries its tags** (so whether it is read or saved) and
  `ingested_at`, when Kiki first stored it. Search can filter on
  `ingested_after` and `ingested_before`.
- **`POST /v1/entries/search/ids`** takes a search and returns only the
  matching IDs, up to 10000 at a time, e.g. every unread entry with the tag
  filter `{"not": "system:read"}`.
- **`POST /v1/entries/batch`** gets up to 1000 entries by ID at once.
- **`POST` and `DELETE /v1/tags/id/{id}/entries`** add a tag to, or remove it
  from, many entries at once, picked by `entry_ids`, `feed_id` and
  `up_to_id`. With a system tag's ID, they mark entries read or unread,
  saved or unsaved.

## Security

A request that carries an [API token](tokens.md) may do only what the
token's scopes allow. By default a token is optional, though: **anybody**
who can reach the API without one can read all feeds and entries, add and
delete feeds and entries, change settings and plugins' config, and do
anything else the API permits. Set `anonymous_access` under `[api]` in
`kiki.toml` to `"read-only"` or `"token-required"` to limit requests
without a token; `GET /v1/access` reports the setting to anyone. See
[Anonymous access](tokens.md#anonymous-access).

By default only your own user can reach the socket. If you widen that, or
put Kiki behind a reverse proxy (see
[Exposing Kiki over the network](deployment.md#exposing-kiki-over-the-network)),
it is up to you to make sure only the people you trust can reach it.
