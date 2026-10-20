#!/bin/sh
# Download a pinned, verified Linux release into the selected application root.
# Run with sh, never source. No login, service start, sudo, or global installation.
set -eu
umask 077
LC_ALL=C; export LC_ALL
unset TAR_OPTIONS
fail() { printf '%s\n' "bootstrap: $*" >&2; exit 1; }
usage() { printf '%s\n' 'Usage: sh bootstrap.sh --root PATH [--version v0.1.0]'; }
root= version=v0.1.0 stage=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --root) [ "$#" -ge 2 ] && [ -n "$2" ] || fail 'expected --root PATH'; root=$2; shift 2 ;;
        --version) [ "$#" -ge 2 ] && [ -n "$2" ] || fail 'expected --version vMAJOR.MINOR.PATCH'; version=$2; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; fail "unknown argument: $1" ;;
    esac
done
[ -n "$root" ] || { usage >&2; fail '--root is required'; }
# Restrict release URLs to ordinary version tags, never arbitrary URLs or paths.
case "$version" in v[0-9]*.[0-9]*.[0-9]*) ;; *) fail 'version must be vMAJOR.MINOR.PATCH' ;; esac
numbers=${version#v}
case "$numbers" in *[!0-9.]*|*..*|.*|*.) fail 'version must be vMAJOR.MINOR.PATCH' ;; esac
major=${numbers%%.*}; rest=${numbers#*.}; minor=${rest%%.*}; patch=${rest#*.}
case "$patch" in *.*) fail 'version must be vMAJOR.MINOR.PATCH' ;; esac
[ -n "$major" ] && [ -n "$minor" ] && [ -n "$patch" ] || fail 'invalid version'
[ "$(uname -s)" = Linux ] || fail 'Linux is required (not macOS or Git Bash/Windows)'
for tool in curl tar sha256sum readlink stat id find cmp mktemp mkdir chmod rm getconf; do
    command -v "$tool" >/dev/null 2>&1 || fail "missing required command: $tool"
done
case "$(tar --version)" in *'GNU tar'*) ;; *) fail 'GNU tar is required for strict archive validation' ;; esac
case "$(uname -m)" in
    x86_64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) fail "unsupported architecture: $(uname -m); use x86_64 or aarch64 Linux" ;;
esac
libc=$(getconf GNU_LIBC_VERSION 2>/dev/null) || fail 'glibc 2.39 or newer is required (Ubuntu 24.04+); musl is unsupported'
case "$libc" in 'glibc '*) ;; *) fail "unsupported libc: $libc; glibc 2.39+ required" ;; esac
libc=${libc#glibc }; libc_major=${libc%%.*}; libc_minor=${libc#*.}
case "$libc_major:$libc_minor" in *[!0-9:]*|:*|*:) fail "unrecognized glibc version: $libc" ;; esac
[ "$libc_major" -gt 2 ] || { [ "$libc_major" -eq 2 ] && [ "$libc_minor" -ge 39 ]; } || fail "glibc $libc is too old; use glibc 2.39+ (Ubuntu 24.04+)"
resolve() {
    case "$1" in /*) candidate=$1 ;; *) candidate=$PWD/$1 ;; esac
    case "$candidate/" in */../*) fail 'parent traversal (..) is not allowed' ;; esac
    while [ "$candidate" != / ] && [ "${candidate%/}" != "$candidate" ]; do candidate=${candidate%/}; done
    part=$candidate
    while [ "$part" != / ]; do
        [ ! -L "$part" ] || fail "symlink path component: $part"
        part=$(dirname -- "$part")
    done
    readlink -m -- "$candidate"
}
[ ! -L "$0" ] || fail 'bootstrap must not be a symlink'
root=$(resolve "$root")
[ "$root" != / ] || fail 'the filesystem root is not an application root'
[ -d "$(dirname -- "$root")" ] || fail 'root parent directory must already exist'
check_dir() {
    [ -d "$1" ] && [ ! -L "$1" ] || fail "not a real directory: $1"
    [ "$(stat -c %u -- "$1")" = "$(id -u)" ] || fail "directory is not owned by the current user: $1"
    mode=$(stat -c %a -- "$1")
    [ $((0$mode & 0022)) -eq 0 ] || fail "directory is group/world writable: $1"
}
[ -e "$root" ] || mkdir -- "$root"
check_dir "$root"
# No installation marker is fabricated. Only the verified installer may create it.
if [ -f "$root/run/installed.sha256" ] && [ ! -L "$root/run" ] && [ ! -L "$root/run/installed.sha256" ]; then
    check_dir "$root/run"
else
    entry=$(find "$root" -mindepth 1 -maxdepth 1 ! -name .bootstrap.sh ! -name tmp -print -quit)
    [ -z "$entry" ] || fail "unknown nonempty target entry: $entry; use a fresh root"
    if [ -e "$root/.bootstrap.sh" ] || [ -L "$root/.bootstrap.sh" ]; then
        [ -f "$root/.bootstrap.sh" ] && [ ! -L "$root/.bootstrap.sh" ] && [ "$(stat -c %h -- "$root/.bootstrap.sh")" -eq 1 ] || fail '.bootstrap.sh must be a regular, single-link file'
        cmp -s -- "$0" "$root/.bootstrap.sh" || fail 'target .bootstrap.sh differs from this bootstrap script'
    fi
    if [ -e "$root/tmp" ] || [ -L "$root/tmp" ]; then
        check_dir "$root/tmp"
        entry=$(find "$root/tmp" -mindepth 1 -print -quit)
        [ -z "$entry" ] || fail "uninstalled root contains temporary files: $entry; inspect and remove only obsolete bootstrap staging before retrying"
    fi
fi
if [ ! -e "$root/tmp" ] && [ ! -L "$root/tmp" ]; then mkdir -- "$root/tmp"; fi
check_dir "$root/tmp"
TMPDIR=$root/tmp; TMP=$TMPDIR; TEMP=$TMPDIR; export TMPDIR TMP TEMP
stage=$(mktemp -d "$root/tmp/bootstrap.XXXXXXXX")
trap 'result=$?; if [ "$result" -ne 0 ]; then printf "%s\n" "bootstrap: failed; diagnostic staging retained at $stage" >&2; fi' 0
trap 'exit 130' INT
trap 'exit 143' TERM
# Downloads and extracted content each get separate new, empty directories.
mkdir -- "$stage/archive" "$stage/checksums" "$stage/package"
archive=nestbot-$version-$arch-linux.tar.gz
base=https://github.com/0pT1ck/NestBot/releases/download/$version
download() {
    printf '%s\n' "Downloading $1" >&2
    curl --disable --fail --show-error --silent --location --proto '=https' --proto-redir '=https' \
        --connect-timeout 30 --max-time 600 --retry 2 --output "$2" "$1" || \
        fail "download failed: $1 (check release assets, TLS, DNS and network access)"
}
download "$base/SHA256SUMS" "$stage/checksums/SHA256SUMS"
download "$base/$archive" "$stage/archive/$archive"
# The release-wide manifest is NOT the package's internal seven-file manifest.
# Parse it, rather than allowing sha256sum -c to follow paths supplied by a download.
seen=' '; selected=; count=0
while IFS=' ' read -r digest name extra || [ -n "$digest$name$extra" ]; do
    [ -z "$extra" ] && [ "${#digest}" -eq 64 ] || fail 'invalid release checksum line'
    case "$digest" in *[!0-9a-f]*) fail 'invalid release SHA-256 digest' ;; esac
    case "$name" in "nestbot-$version-x86_64-linux.tar.gz"|"nestbot-$version-aarch64-linux.tar.gz") ;; *) fail "unexpected release checksum path: $name" ;; esac
    case "$seen" in *" $name "*) fail "duplicate release checksum: $name" ;; esac
    seen="$seen$name "; count=$((count + 1))
    if [ "$name" = "$archive" ]; then selected=$digest; fi
done < "$stage/checksums/SHA256SUMS"
[ "$count" -eq 2 ] && [ -n "$selected" ] || fail 'release SHA256SUMS must list both Linux architectures'
actual=$(sha256sum -- "$stage/archive/$archive"); actual=${actual%% *}
[ "$actual" = "$selected" ] || fail "release archive checksum mismatch: $archive"
files='bin/nestbot config/config.example.toml scripts/install.sh scripts/start.sh scripts/stop.sh scripts/status.sh doc/deployment.md'
# GNU tar's escaped, numeric listing has fixed columns. Whitelisted paths contain
# no spaces or escape characters. Reject links, special files, duplicates, aliases,
# traversal and absolute names BEFORE extracting any member or executing anything.
tar --list --verbose --numeric-owner --full-time --quoting-style=escape \
    --gzip --file "$stage/archive/$archive" > "$stage/tar.list" || fail 'cannot list release archive'
seen=' '; count=0
while IFS=' ' read -r mode owner size date time name extra; do
    [ -z "$extra" ] && [ -n "$name" ] || fail 'invalid archive listing or unsafe filename'
    case "$seen" in *" $name "*) fail "duplicate archive member: $name" ;; esac
    seen="$seen$name "
    case "$mode:$name" in
        d*:bin/|d*:config/|d*:scripts/|d*:doc/) ;;
        -*:*)
            case " $files SHA256SUMS " in *" $name "*) ;; *) fail "unexpected archive path: $name" ;; esac
            count=$((count + 1)) ;;
        *) fail "unsupported archive type or directory: $mode $name" ;;
    esac
done < "$stage/tar.list"
[ "$count" -eq 8 ] || fail 'archive must contain exactly seven release files and SHA256SUMS'
tar --extract --gzip --file "$stage/archive/$archive" --directory "$stage/package" \
    --no-same-owner --no-same-permissions --keep-old-files --delay-directory-restore || fail 'archive extraction failed'
package=$stage/package
seen=' '; count=0
while IFS=' ' read -r digest name extra || [ -n "$digest$name$extra" ]; do
    [ -z "$extra" ] && [ "${#digest}" -eq 64 ] || fail 'invalid package checksum line'
    case "$digest" in *[!0-9a-f]*) fail 'invalid package SHA-256 digest' ;; esac
    case " $files " in *" $name "*) [ -n "$name" ] || fail 'empty package checksum path' ;; *) fail "unexpected package checksum path: $name" ;; esac
    case "$seen" in *" $name "*) fail "duplicate package checksum: $name" ;; esac
    seen="$seen$name "; count=$((count + 1))
    [ -f "$package/$name" ] && [ ! -L "$package/$name" ] || fail "not a regular package file: $name"
done < "$package/SHA256SUMS"
[ "$count" -eq 7 ] || fail 'package manifest must contain all seven release files'
(cd -- "$package" && sha256sum --strict -c SHA256SUMS) || fail 'internal package checksum verification failed'
# This explicit installer agreement recognizes only this root-local staging layout;
# it does not forge an existing-install marker or bypass preservation checks.
sh "$package/scripts/install.sh" --root "$root" --bootstrap-stage "$stage"
rm -rf -- "$stage"
printf '%s\n' "Bootstrap completed for $version ($arch). No login or service start was performed."
