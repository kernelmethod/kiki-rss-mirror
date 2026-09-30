# kiki-rss

Kiki is an RSS/Atom reader _engine_. It is intended to provide a service
boundary between the feed fetching and filtering component of an RSS reader and
the user interface. It is also a reusable component for any applications that
needs to regularly consume RSS feeds.

## Explicit non-goals

Kiki is explicitly _not_ an RSS/Atom reader. That means that the following are
out of scope for this project:

* Any sort of web interface, CLI, application, etc. to manage Kiki. Kiki is
  intended to provide common backend infrastructure for RSS readers.
* Favorite entries and entry read status. All per-entry user metadata is
  mediated through the use of tags, which Kiki clients can use to implement
  features like this. Kiki distinguishes two kinds of tag:
  * **User tags** are created, renamed, and deleted by the user (or by
    scripts and OPML imports).
  * **System tags** are built in and record common entry metadata:
    `system:read`, `system:saved`, and `system:hidden`. They cannot be renamed
    or deleted, and are applied to entries with
    `PUT`/`DELETE /v1/entries/id/{id}/system-tags/{name}`. The `system:`
    prefix is reserved, so user tags cannot start with it. Entries tagged
    `system:hidden` are left out of entry lists unless asked for with
    `include_hidden=true`.

The following is currently out-of-scope, although these features may be
reconsidered some day:

* Any sort of user model or permission structure.
* API authentication. _All_ requests to the API are unauthenticated; see
  [Security](#Security).

## Syncing entries

Clients that keep their own copy of Kiki's entries, such as a sync layer for
another reader's API, can fetch just what changed:

* **Entry IDs only ever increase**, even after entries are deleted, so the
  highest ID a client has seen marks where it left off. The entry listings
  (`GET /v1/entries`, `/v1/feeds/id/{id}/entries`,
  `/v1/tags/id/{id}/entries`) and `POST /v1/entries/search` take
  `since_id` and `max_id`, and sort by ID with `sort=id` or `sort=id_desc`.
  Passing the last ID of one page as the next page's `since_id` pages
  through entries without skipping any, unlike `offset`.
* **Every entry carries its tags** (so whether it is read or saved) and
  `ingested_at`, when Kiki first stored it. Search can filter on
  `ingested_after` and `ingested_before`.
* **`POST /v1/entries/search/ids`** takes a search and returns only the
  matching IDs, up to 10000 at a time, e.g. every unread entry with the tag
  filter `{"not": "system:read"}`.
* **`POST /v1/entries/batch`** gets up to 1000 entries by ID at once.
* **`POST` and `DELETE /v1/tags/id/{id}/entries`** add a tag to, or remove it
  from, many entries at once, picked by `entry_ids`, `feed_id` and
  `up_to_id`. With a system tag's ID, they mark entries read or unread,
  saved or unsaved.

## Security

The Kiki API does not implement any sort of authentication. This means that
**anybody** who can reach the API can retrieve all feed and entry information,
add and delete feeds/entires, retrieve settings, and perform
any other action permitted by the API.

It is the deployers' responsibility to ensure that Kiki is only accessible to
permitted users.
