#!/bin/sh
set -eu
umask 077
fail() { printf '%s\n' "start: $*" >&2; exit 1; }
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
        --help|-h) printf '%s\n' 'Usage: sh scripts/start.sh [--root PATH]'; exit 0 ;;
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
# Rust validates managed paths, config, and permissions without logging in.
"$root/bin/nestbot" --root "$root" check
set +e
"$root/bin/nestbot" --root "$root" status
result=$?
set -e
case "$result" in
    0) printf '%s\n' 'Already running.'; exit 0 ;;
    3) ;;
    *) fail 'unable to determine instance status' ;;
esac
TMPDIR=$root/tmp; TMP=$TMPDIR; TEMP=$TMPDIR
export TMPDIR TMP TEMP
# Rust owns a bounded logs/ writer, including startup errors. Never create nohup.out.
nohup "$root/bin/nestbot" --root "$root" run </dev/null >/dev/null 2>&1 &
# No PID file and no signals: the Rust lock/status protocol owns process identity.
attempt=0
while [ "$attempt" -lt 10 ]; do
    sleep 1
    set +e
    "$root/bin/nestbot" --root "$root" status >/dev/null 2>&1
    result=$?
    set -e
    case "$result" in
        0)
            # Avoid declaring success for a process that fails immediately.
            sleep 1
            if "$root/bin/nestbot" --root "$root" status; then
                printf '%s\n' 'Running (lock held; this is not a Telegram connectivity health check).'
                exit 0
            fi
            fail 'process exited during startup; inspect bounded logs/'
            ;;
        3) ;;
        *) fail 'status failed during startup; inspect logs/ and root permissions' ;;
    esac
    attempt=$((attempt + 1))
done
fail 'not running after 10 seconds; inspect bounded logs/. No process was killed; use status/stop before retrying.'
