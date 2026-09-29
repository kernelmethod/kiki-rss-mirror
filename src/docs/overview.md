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
    `include_hidden=true`. Entries tagged `system:read` are listed unless
    left out with `exclude_read=true`; the web UI (`kiki web`) leaves them
    out by default, and its "Filter" menu has a checkbox to show them.

The following is currently out-of-scope, although these features may be
reconsidered some day:

* Any sort of user model or permission structure.
* API authentication. _All_ requests to the API are unauthenticated; see
  [Security](#Security).

## Security

The Kiki API does not implement any sort of authentication. This means that
**anybody** who can reach the API can retrieve all feed and entry information,
add and delete feeds/entires, retrieve settings, and perform
any other action permitted by the API.

It is the deployers' responsibility to ensure that Kiki is only accessible to
permitted users.
