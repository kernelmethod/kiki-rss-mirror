# Sanitizing entries' HTML

The `sanitize` plugin rewrites the HTML content of new entries as they
arrive, so that what Kiki stores, and hands to [API](../api.md) clients, is
safe to show:

- It keeps an allowlist of formatting elements, such as `<p>`, `<a>`,
  `<img>`, lists and tables. Any other element is unwrapped: its tags are
  removed and its content kept.
- It removes scripts, styles, frames, embedded objects, SVG, MathML and
  form controls together with their content, and removes comments.
- It keeps an allowlist of attributes, such as `href` on links and `src`,
  `alt`, `width` and `height` on images. Every other attribute is removed,
  including `style`, `class` and event handlers such as `onclick`.
- It removes links and image sources whose URLs use a scheme other than
  `http`, `https` or `mailto`, such as `javascript:`, however the scheme is
  disguised. Relative URLs are kept. An image left without a source is
  removed.

It is [installed by default](index.md) and works without any
configuration. It only sanitizes entries as they are downloaded, not the
ones already stored. The [web UI](../web-ui.md) sanitizes content again
before showing it, whatever this plugin keeps.

## Changing what it keeps

To keep more elements or attributes, or fewer, start from its defaults:

```bash
kiki plugin config get sanitize --defaults > sanitize.toml
# edit sanitize.toml, then:
kiki plugin config set sanitize sanitize.toml
```

| Setting       | Meaning |
| ------------- | ------- |
| `elements`    | The elements to keep. Any other element is unwrapped. |
| `drop`        | Elements to remove together with their content, rather than unwrap: `audio`, `button`, `canvas`, `input`, `select` and `video` by default. |
| `attributes`  | The attributes to keep. A bare name, such as `"title"`, keeps the attribute on every element; `"element:name"`, such as `"a:href"`, keeps it on that element only. |
| `url_schemes` | The schemes URL attributes (`href`, `src`, `cite`, …) may use. |

Some things can't be configured away: scripts, styles, `<iframe>`,
`<object>`, `<embed>`, `<svg>`, `<math>`, `<base>`, `<meta>`, `<link>` and
`<template>`, and elements whose content isn't parsed as HTML, such as
`<noscript>` and `<textarea>`, are always removed with their content, and
event handler attributes are always removed.

For example, to keep videos (with their sources and posters), and the
`class` attribute on every element:

```bash
kiki plugin config get sanitize --defaults > sanitize.toml
# in sanitize.toml: add "video" and "source" to `elements`, remove "video"
# from `drop`, and add "class", "video:src", "video:poster", "video:controls"
# and "source:src" to `attributes`
kiki plugin config set sanitize sanitize.toml
```

## Plugin order

Plugins run in the order of their directory names, so `sanitize` runs after
`filter`, `auto-tag` and `privacy`. Plugins whose
directory names sort after `sanitize` see the sanitized content, and can
add markup back; a plugin of your own that should run on unsanitized
content needs a name that sorts before it.

The plugin is written in Rust, as a [WebAssembly plugin](../writing-wasm-plugins.md);
see [`plugins/sanitize-src`](https://github.com/kernelmethod/kiki-rss/tree/main/plugins/sanitize-src)
for the details. It parses HTML with the same rewriter as
[`kiki.html`](../writing-plugins.md#rewriting-html), which plugins of your own
can use to rewrite entries' HTML too. Versions before 2.0.0 were written in
Lua, and took the same configuration.
