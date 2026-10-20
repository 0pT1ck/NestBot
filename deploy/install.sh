#!/bin/sh
# Local, offline installation only. Run with sh; never source this file.
set -eu
umask 077
fail() { printf '%s\n' "install: $*" >&2; exit 1; }
usage() { printf '%s\n' 'Usage: sh scripts/install.sh [--root PATH]'; }
[ "$(uname -s)" = Linux ] || fail 'installation requires Linux (not Git Bash/Windows)'
for tool in readlink stat id find sha256sum cmp cp chmod mkdir; do
    command -v "$tool" >/dev/null 2>&1 || fail "missing required command: $tool"
done
# Refuse symlink components rather than allowing an alias outside the chosen root.
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
[ ! -L "$0" ] || fail 'the installer must not be a symlink'
script_dir=$(resolve "$(dirname -- "$0")")
source_root=$(dirname -- "$script_dir")
root=$source_root
bootstrap_stage=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h) usage; exit 0 ;;
        --root) [ "$#" -ge 2 ] && [ -n "$2" ] || fail 'expected --root PATH'; root=$(resolve "$2"); shift 2 ;;
        --bootstrap-stage) [ "$#" -ge 2 ] && [ -n "$2" ] && [ -z "$bootstrap_stage" ] || fail 'expected one --bootstrap-stage PATH'; bootstrap_stage=$(resolve "$2"); shift 2 ;;
        *) usage >&2; exit 1 ;;
    esac
done
if [ -n "$bootstrap_stage" ]; then
    [ "$(dirname -- "$bootstrap_stage")" = "$root/tmp" ] || fail 'bootstrap staging must be directly under root/tmp'
    stage_name=$(basename -- "$bootstrap_stage")
    case "$stage_name" in bootstrap.????????) ;; *) fail 'invalid bootstrap stage name' ;; esac
    case "$stage_name" in *[!a-zA-Z0-9.]*) fail 'invalid bootstrap stage characters' ;; esac
    [ "$source_root" = "$bootstrap_stage/package" ] || fail 'installer is not in the declared bootstrap package'
fi
[ "$root" != / ] || fail 'the filesystem root is not an application root'
# A deployment must not be nested inside its source package, including tmp/.
# Copying is a fixed file list, but nested roots would share recursive safety checks.
case "$root" in "$source_root"/*) fail 'target must not be a child of the source package; use the package root or a separate directory' ;; esac
[ -d "$(dirname -- "$root")" ] || fail 'target parent directory must already exist'
[ -d "$source_root" ] || fail 'missing package directory'
files='bin/nestbot config/config.example.toml scripts/install.sh scripts/start.sh scripts/stop.sh scripts/status.sh doc/deployment.md'
check_tree() {
    tree=$1
    [ ! -L "$tree" ] || fail "symlink root: $tree"
    [ "$(stat -c %u -- "$tree")" = "$(id -u)" ] || fail "directory is not owned by the current user: $tree"
    mode=$(stat -c %a -- "$tree")
    [ $((0$mode & 0022)) -eq 0 ] || fail "directory must not be group/world writable: $tree"
    found=$(find "$tree" -type l -print -quit)
    [ -z "$found" ] || fail "symlinks are not supported in an installation: $found"
    found=$(find "$tree" ! -uid "$(id -u)" -print -quit)
    [ -z "$found" ] || fail "entry is not owned by the current user: $found"
    found=$(find "$tree" -perm /0022 -print -quit)
    [ -z "$found" ] || fail "entry is group/world writable: $found"
    found=$(find "$tree" -type f ! -links 1 -print -quit)
    [ -z "$found" ] || fail "hard-linked files are not supported: $found"
}
verify_package() {
    tree=$1
    [ -f "$tree/SHA256SUMS" ] || fail 'missing SHA256SUMS'
    count=0
    seen=' '
    while IFS=' ' read -r digest name extra || [ -n "$digest$name$extra" ]; do
        [ -z "$extra" ] || fail 'invalid manifest line'
        [ "${#digest}" -eq 64 ] || fail 'invalid SHA-256 digest length'
        case "$digest" in *[!0-9a-f]*) fail 'invalid SHA-256 digest' ;; esac
        case " $files " in *" $name "*) [ -n "$name" ] || fail 'empty manifest path' ;; *) fail "unexpected manifest path: $name" ;; esac
        case "$seen" in *" $name "*) fail "duplicate manifest path: $name" ;; esac
        seen="$seen$name "
        [ -f "$tree/$name" ] && [ ! -L "$tree/$name" ] || fail "missing regular package file: $name"
        count=$((count + 1))
    done < "$tree/SHA256SUMS"
    [ "$count" -eq 7 ] || fail 'manifest must contain all seven release files'
    (cd -- "$tree" && sha256sum --strict -c SHA256SUMS) || fail 'package checksum verification failed'
}
check_tree "$source_root"
verify_package "$source_root"
[ -x "$source_root/bin/nestbot" ] || fail 'package binary is not executable'
version=$("$source_root/bin/nestbot" --version) || fail 'cannot execute the Linux package binary (check architecture/libc)'
case "$version" in *-linux) ;; *) fail "binary does not report a Linux platform: $version" ;; esac
# Recognize only a pristine unpacked package or a previously recorded install.
existing=0
if [ -e "$root" ]; then
    [ -d "$root" ] || fail 'target exists but is not a directory'
    check_tree "$root"
    if [ -f "$root/run/installed.sha256" ]; then
        existing=1
        cmp -s -- "$root/run/installed.sha256" "$source_root/SHA256SUMS" || fail 'different release: upgrades/replacements are not supported'
        cmp -s -- "$root/SHA256SUMS" "$source_root/SHA256SUMS" || fail 'installed manifest changed; replacement is not supported'
        verify_package "$root"
        for name in $files; do
            cmp -s -- "$root/$name" "$source_root/$name" || fail "installed file changed: $name; replacement is not supported"
        done
        set +e
        "$root/bin/nestbot" --root "$root" status
        result=$?
        set -e
        case "$result" in 3) ;; 0) fail 'instance is running; stop it before rerunning installation' ;; *) fail 'unable to determine instance status' ;; esac
    elif [ -n "$bootstrap_stage" ]; then
        # Only the explicitly declared first-install staging layout is accepted.
        unexpected=$(find "$root" -mindepth 1 -maxdepth 1 ! -name '.bootstrap.sh' ! -name 'tmp' -print -quit)
        [ -z "$unexpected" ] || fail "unknown bootstrap root entry: $unexpected"
        if [ -e "$root/.bootstrap.sh" ]; then
            [ -f "$root/.bootstrap.sh" ] || fail 'bootstrap entry is not a regular file'
        fi
        [ -d "$root/tmp" ] && [ -d "$bootstrap_stage" ] || fail 'missing bootstrap staging directory'
        unexpected=$(find "$root/tmp" -mindepth 1 -maxdepth 1 ! -name "$stage_name" -print -quit)
        [ -z "$unexpected" ] || fail "unrelated staging entry: $unexpected"
        check_tree "$bootstrap_stage"
    elif [ "$root" = "$source_root" ]; then
        # A freshly unpacked package is intentionally nonempty.
        unexpected=$(find "$root" -mindepth 1 ! -type d ! -type f -print -quit)
        [ -z "$unexpected" ] || fail "unsupported package entry: $unexpected"
        unexpected=$(cd -- "$root" && find . -mindepth 1 ! \( \
            -path './bin' -o -path './config' -o -path './scripts' -o -path './doc' -o \
            -path './SHA256SUMS' -o -path './bin/nestbot' -o -path './config/config.example.toml' -o \
            -path './scripts/install.sh' -o -path './scripts/start.sh' -o \
            -path './scripts/stop.sh' -o -path './scripts/status.sh' -o -path './doc/deployment.md' \
            \) -print -quit)
        [ -z "$unexpected" ] || fail "unknown nonempty target entry: $unexpected"
    else
        entry=$(find "$root" -mindepth 1 -print -quit)
        [ -z "$entry" ] || fail 'unknown nonempty target directory; refusing to overwrite'
    fi
fi
if [ "$existing" -eq 0 ]; then
    mkdir -p -- "$root"
    chmod 700 -- "$root"
    for dir in bin config scripts doc data logs tmp run backups; do
        mkdir -p -- "$root/$dir"
        chmod 700 -- "$root/$dir"
    done
    if [ "$root" != "$source_root" ]; then
        for name in $files SHA256SUMS; do
            cp -- "$source_root/$name" "$root/$name"
        done
    fi
    chmod 700 -- "$root/bin/nestbot" "$root/scripts/"*.sh
    chmod 600 -- "$root/config/config.example.toml" "$root/doc/deployment.md" "$root/SHA256SUMS"
    # Record the verified release before init so an interrupted init can be retried.
    cp -- "$root/SHA256SUMS" "$root/run/installed.sha256"
    chmod 600 -- "$root/run/installed.sha256"
fi
# init uses create_new for config.toml and must preserve existing state.
TMPDIR=$root/tmp; TMP=$TMPDIR; TEMP=$TMPDIR
export TMPDIR TMP TEMP
"$root/bin/nestbot" --root "$root" init
printf '%s\n' "Installed $version in $root" \
    'Next: edit config/config.toml, run bin/nestbot --root PATH check, then login and scripts/start.sh.' \
    'Installation does not validate credentials, log in, start the service, or enable autostart.'
