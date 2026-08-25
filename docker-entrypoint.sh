#!/bin/sh
set -eu

# KIKI_DATA_DIR is the pre-0.7 name for KIKI_HOME. Honour it so existing
# `docker run -e KIKI_DATA_DIR=...` invocations keep working, but say so.
if [ -n "${KIKI_DATA_DIR:-}" ]; then
    if [ -n "${KIKI_HOME:-}" ]; then
        echo "kiki: KIKI_DATA_DIR is deprecated and KIKI_HOME is set; ignoring KIKI_DATA_DIR" >&2
    else
        echo "kiki: KIKI_DATA_DIR is deprecated, use KIKI_HOME instead" >&2
        KIKI_HOME="$KIKI_DATA_DIR"
    fi
fi

: "${KIKI_HOME:=/data}"
: "${KIKI_BIND:=0.0.0.0}"
: "${KIKI_PORT:=8000}"

# Every kiki subcommand resolves its paths out of $KIKI_HOME.
export KIKI_HOME

# Idempotent: --check is a no-op when kiki.db already exists.
kiki init --check "$KIKI_HOME"

if [ "${1:-serve}" = "serve" ]; then
    shift 2>/dev/null || true
    exec kiki serve --bind "$KIKI_BIND" --port "$KIKI_PORT" "$@"
fi

exec kiki "$@"
