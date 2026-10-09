#!/bin/sh
set -eu

# Resolve paths from this release directory, not the caller's working directory.
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$ROOT"
umask 077
mkdir -p .local/tmp .local/log
export TMPDIR="$ROOT/.local/tmp"
export SQLITE_TMPDIR="$TMPDIR"
# Do not leave core dumps outside the application directory on a crash.
ulimit -c 0
exec "$ROOT/nestbot" --config "$ROOT/config/nestbot.toml" --env-file "$ROOT/config/secrets.env" "$@"
