#!/bin/sh
set -eu

: "${KIKI_DATA_DIR:=/data}"
: "${KIKI_BIND:=0.0.0.0}"
: "${KIKI_PORT:=8000}"

# Idempotent: --check is a no-op when kiki.db already exists.
kiki init --check "$KIKI_DATA_DIR"

if [ "${1:-serve}" = "serve" ]; then
    shift 2>/dev/null || true
    exec kiki serve --bind "$KIKI_BIND" --port "$KIKI_PORT" "$@"
fi

exec kiki "$@"
