#!/bin/sh
# Package an already built, native Linux release. No downloads and no build here.
set -eu
umask 077
fail() { printf '%s\n' "package: $*" >&2; exit 1; }
[ "$(uname -s)" = Linux ] || fail 'packaging requires Linux; Windows binaries are not Linux releases'
for tool in readlink file sha256sum tar mktemp cp chmod mkdir; do
    command -v "$tool" >/dev/null 2>&1 || fail "missing required command: $tool"
done
[ ! -L "$0" ] || fail 'script symlinks are not supported'
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo=$(dirname -- "$script_dir")
binary=$repo/target/release/nestbot
if [ "$#" -gt 0 ]; then
    case "$1" in
        --help|-h) printf '%s\n' 'Usage: sh scripts/package.sh [--binary target/TRIPLE/release/nestbot]'; exit 0 ;;
        --binary) [ "$#" -eq 2 ] && [ -n "$2" ] || fail 'expected --binary PATH'; binary=$2 ;;
        *) fail 'expected --binary PATH' ;;
    esac
fi
case "$binary" in /*) ;; *) binary=$PWD/$binary ;; esac
binary=$(readlink -f -- "$binary") || fail 'binary does not exist'
case "$binary" in "$repo"/target/release/nestbot|"$repo"/target/*/release/nestbot) ;; *) fail 'binary must be a release artifact inside this repository target/' ;; esac
[ -f "$binary" ] && [ -x "$binary" ] || fail 'missing executable release binary; build on Linux first'
case "$(file -b -- "$binary")" in ELF\ *) ;; *) fail 'release binary is not an ELF executable' ;; esac
version=$("$binary" --version) || fail 'release binary cannot run on this Linux host (architecture/libc mismatch)'
case "$version" in 'nestbot '*-linux) ;; *) fail "binary does not report a Linux platform: $version" ;; esac
arch=$(uname -m)
case "$arch" in
    x86_64) platform=x86_64-linux ;;
    aarch64|arm64) platform=aarch64-linux ;;
    *) fail "unvalidated host architecture: $arch" ;;
esac
case "$version" in *" $platform") ;; *) fail "binary platform does not match host: $version" ;; esac
# Guard all output ancestors before mkdir/mktemp. Never honor an external target dir.
for path in "$repo/target" "$repo/target/package"; do
    [ ! -L "$path" ] || fail "output directory is a symlink: $path"
    [ ! -e "$path" ] || [ -d "$path" ] || fail "not a directory: $path"
done
mkdir -p -- "$repo/target/package"
TMPDIR=$repo/target/package; TMP=$TMPDIR; TEMP=$TMPDIR
export TMPDIR TMP TEMP
stage=$(mktemp -d "$repo/target/package/nestbot-$platform.XXXXXXXX")
archive=$repo/target/$(basename -- "$stage").tar.gz
# An interrupted package is left in target/package for inspection, never published as complete.
for dir in bin config scripts doc; do mkdir -- "$stage/$dir"; done
cp -- "$binary" "$stage/bin/nestbot"
cp -- "$repo/config/config.example.toml" "$stage/config/config.example.toml"
for script in install start stop status; do
    cp -- "$repo/deploy/$script.sh" "$stage/scripts/$script.sh"
done
cp -- "$repo/doc/deployment.md" "$stage/doc/deployment.md"
chmod 700 -- "$stage/bin/nestbot" "$stage/scripts/"*.sh
files='bin/nestbot config/config.example.toml scripts/install.sh scripts/start.sh scripts/stop.sh scripts/status.sh doc/deployment.md'
(cd -- "$stage" && sha256sum $files > SHA256SUMS && sha256sum --strict -c SHA256SUMS)
# Archive has no containing directory: extract it only into a new empty deployment root.
# noclobber also protects against a pre-created archive/symlink at the output name.
(set -C; tar -czf - -C "$stage" bin config scripts doc SHA256SUMS > "$archive")
printf '%s\n' "Release: $version" "Package directory: $stage" "Archive: $archive"
sha256sum -- "$archive"
printf '%s\n' 'Verify libc/runtime requirements on the target VPS. Packaging is not Linux deployment acceptance.'
