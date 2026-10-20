#!/bin/sh
set -eu
umask 077
fail() { printf '%s\n' "status: $*" >&2; exit 1; }
[ "$(uname -s)" = Linux ] || fail 'Linux is required'
[ ! -L "$0" ] || fail 'script symlinks are not supported'
script_dir=$(dirname -- "$0")
case "$script_dir" in /*) ;; *) script_dir=$PWD/$script_dir ;; esac
part=$script_dir
while [ "$part" != / ]; do
    [ ! -L "$part" ] || fail "symlink script directory: $part"
    part=$(dirname -- "$part")
done
root=$(dirname -- "$(readlink -m -- "$script_dir")")
if [ "$#" -gt 0 ]; then
    case "$1" in
        --help|-h) printf '%s\n' 'Usage: sh scripts/status.sh [--root PATH]'; exit 0 ;;
        --root) [ "$#" -eq 2 ] && [ -n "$2" ] || fail 'expected --root PATH'; root=$2 ;;
        *) fail 'expected --root PATH' ;;
    esac
fi
case "$root" in /*) ;; *) root=$PWD/$root ;; esac
case "$root/" in */../*) fail 'parent traversal (..) is not allowed' ;; esac
while [ "$root" != / ] && [ "${root%/}" != "$root" ]; do root=${root%/}; done
part=$root
while [ "$part" != / ]; do
    [ ! -L "$part" ] || fail "symlink path component: $part"
    part=$(dirname -- "$part")
done
root=$(readlink -m -- "$root")
[ "$root" != / ] && [ -x "$root/bin/nestbot" ] || fail 'not an initialized application root'
[ ! -L "$root/bin" ] && [ ! -L "$root/bin/nestbot" ] || fail 'binary symlinks are not supported'
# Exit codes belong to Rust: 0 running, 3 stopped, other nonzero means error.
exec "$root/bin/nestbot" --root "$root" status
