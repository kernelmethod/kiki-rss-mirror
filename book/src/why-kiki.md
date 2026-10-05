# Why Kiki?

## Design principles

## Anti-principles

Kiki _is not_ designed for multi-user installations. There is a single
set of feeds, a single set of entries, and a single set of plugins.

## Comparison with other aggregators

Most feed aggregators ship with their engine (the component that manages
feeds and entries) and their frontend as one fused unit. In Kiki, the
engine is a basic HTTP API which other developers can build on top of
to create the frontend of their choice, although Kiki also ships with
its own [web-based UI](web-ui.md).

