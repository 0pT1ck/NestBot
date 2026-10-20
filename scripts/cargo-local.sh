#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
export CARGO_HOME="$root/tmp/cargo-home"
export CARGO_TARGET_DIR="$root/target"
export TMPDIR="$root/tmp/build"
export TEMP="$TMPDIR" TMP="$TMPDIR"
export CARGO_HTTP_MULTIPLEXING=false
mkdir -p "$CARGO_HOME" "$TMPDIR" "$CARGO_TARGET_DIR"
cd "$root"
exec cargo "$@"
