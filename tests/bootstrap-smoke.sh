#!/bin/sh
# Offline shell smoke tests: fake network/platform/CLI, real archive and installer.
# This is NOT a real Linux binary, glibc, Telegram, or VPS acceptance test.
set -eu
umask 077
LC_ALL=C; export LC_ALL
unset TAR_OPTIONS
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
repo=$(dirname -- "$script_dir")
fail() { printf '%s\n' "bootstrap-smoke: $*" >&2; exit 1; }
for tool in tar sha256sum mktemp cp chmod cmp; do
    command -v "$tool" >/dev/null 2>&1 || fail "missing tool: $tool"
done
[ ! -L "$repo/tmp" ] || fail 'repo/tmp must not be a symlink'
mkdir -p -- "$repo/tmp"
work=$(mktemp -d "$repo/tmp/bootstrap-smoke.XXXXXXXX")
trap 'result=$?; if [ "$result" -eq 0 ]; then rm -rf -- "$work"; else printf "%s\n" "bootstrap-smoke: fixtures retained at $work" >&2; fi' 0
mkdir -- "$work/stubs" "$work/fixtures" "$work/roots"
# Scripts live outside the directories containing simulated downloaded artifacts.
cat > "$work/stubs/uname" <<'STUB'
#!/bin/sh
case "$1" in -s) printf '%s\n' "${TEST_OS:-Linux}" ;; -m) printf '%s\n' "${TEST_ARCH:-x86_64}" ;; *) exit 1 ;; esac
STUB
cat > "$work/stubs/getconf" <<'STUB'
#!/bin/sh
printf '%s\n' "${TEST_LIBC:-glibc 2.39}"
STUB
cat > "$work/stubs/curl" <<'STUB'
#!/bin/sh
set -eu
out= url=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output) out=$2; shift 2 ;;
        --proto|--proto-redir|--connect-timeout|--max-time|--retry) shift 2 ;;
        --*) shift ;;
        *) url=$1; shift ;;
    esac
done
case "$url" in https://github.com/0pT1ck/NestBot/releases/download/v0.1.0/*) ;; *) printf '%s\n' "unexpected download URL: $url" >&2; exit 9 ;; esac
[ "${TEST_CURL_FAIL:-0}" -eq 0 ] || { printf '%s\n' 'simulated curl TLS failure' >&2; exit 60; }
[ -n "$out" ] && [ -n "$url" ]
printf '%s\n' "$out" >> "$TEST_DOWNLOAD_LOG"
cp -- "$TEST_FIXTURE/${url##*/}" "$out"
STUB
chmod 700 -- "$work/stubs/"*
PATH=$work/stubs:$PATH; export PATH
TEST_DOWNLOAD_LOG=$work/downloads.log; export TEST_DOWNLOAD_LOG
files='bin/nestbot config/config.example.toml scripts/install.sh scripts/start.sh scripts/stop.sh scripts/status.sh doc/deployment.md'
fixture() {
    name=$1
    fixture_root=$work/fixtures/$name
    mkdir -- "$fixture_root" "$fixture_root/source"
    source=$fixture_root/source
    for dir in bin config scripts doc; do mkdir -- "$source/$dir"; done
    cat > "$source/bin/nestbot" <<'BINARY'
#!/bin/sh
set -eu
if [ "$1" = --version ]; then printf '%s\n' "nestbot 0.1.0 ${TEST_ARCH:-x86_64}-linux"; exit 0; fi
[ "$1" = --root ]
root=$2
case "$3" in
    status) exit 3 ;;
    init)
        if [ ! -f "$root/config/config.toml" ]; then printf '%s\n' 'test-configuration' > "$root/config/config.toml"; fi
        printf '%s\n' init >> "$root/run/test-init.log" ;;
    *) exit 9 ;;
esac
BINARY
    cp -- "$repo/deploy/install.sh" "$source/scripts/install.sh"
    for item in start stop status; do printf '%s\n' '#!/bin/sh' 'exit 99' > "$source/scripts/$item.sh"; done
    printf '%s\n' 'example' > "$source/config/config.example.toml"
    printf '%s\n' 'fixture documentation' > "$source/doc/deployment.md"
    chmod 700 -- "$source/bin/nestbot" "$source/scripts/"*.sh
    (cd -- "$source" && sha256sum --text $files > SHA256SUMS)
}
pack() {
    (cd -- "$source" && tar -czf "$fixture_root/nestbot-v0.1.0-x86_64-linux.tar.gz" bin config scripts doc SHA256SUMS)
    cp -- "$fixture_root/nestbot-v0.1.0-x86_64-linux.tar.gz" "$fixture_root/nestbot-v0.1.0-aarch64-linux.tar.gz"
    (cd -- "$fixture_root" && sha256sum --text nestbot-v0.1.0-x86_64-linux.tar.gz nestbot-v0.1.0-aarch64-linux.tar.gz > SHA256SUMS)
}
select_fixture() { TEST_FIXTURE=$work/fixtures/$1; export TEST_FIXTURE; }
new_root() {
    target=$work/roots/$1
    mkdir -- "$target"
    cp -- "$repo/deploy/bootstrap.sh" "$target/.bootstrap.sh"
}
run_bootstrap() { sh "$target/.bootstrap.sh" --root "$target" > "$work/result.log" 2>&1; }
expect_failure() {
    if run_bootstrap; then fail "unexpected success: $1"; fi
    if [ "${2:-pristine}" = pristine ]; then
        [ ! -e "$target/run/installed.sha256" ] || fail "unsafe input reached installation: $1"
        [ ! -e "$target/bin" ] || fail "unsafe input copied files: $1"
    fi
    printf '%s\n' "PASS $1"
}
fixture good; pack; select_fixture good
new_root first-install
run_bootstrap || { cat "$work/result.log" >&2; fail 'first install failed'; }
[ -f "$target/run/installed.sha256" ] && [ -f "$target/config/config.toml" ] || fail 'missing initialized install'
printf '%s\n' 'preserve credentials' > "$target/config/config.toml"
printf '%s\n' 'preserve session' > "$target/data/session.fixture"
run_bootstrap || { cat "$work/result.log" >&2; fail 'same-package rerun failed'; }
[ "$(cat "$target/config/config.toml")" = 'preserve credentials' ] || fail 'configuration replaced'
[ "$(cat "$target/data/session.fixture")" = 'preserve session' ] || fail 'session replaced'
[ -z "$(find "$target/tmp" -mindepth 1 -print -quit)" ] || fail 'successful staging was not cleaned'
printf '%s\n' 'PASS first install and same-package preservation'
installed=$target
fixture different
printf '%s\n' 'different release content' > "$source/doc/deployment.md"
(cd -- "$source" && sha256sum --text $files > SHA256SUMS)
pack; select_fixture different; target=$installed
expect_failure different-package installed
[ "$(cat "$target/config/config.toml")" = 'preserve credentials' ] || fail 'rejected release modified configuration'
select_fixture good; new_root arm64
TEST_ARCH=aarch64; export TEST_ARCH
run_bootstrap || { cat "$work/result.log" >&2; fail 'aarch64 selection failed'; }
unset TEST_ARCH
printf '%s\n' 'PASS aarch64 asset selection'
new_root unsupported; TEST_ARCH=riscv64; export TEST_ARCH
expect_failure unsupported-architecture
unset TEST_ARCH
new_root old-libc; TEST_LIBC='glibc 2.38'; export TEST_LIBC
expect_failure old-glibc
unset TEST_LIBC
new_root non-linux; TEST_OS=Darwin; export TEST_OS
expect_failure non-linux
unset TEST_OS
new_root user-data
printf '%s\n' precious > "$target/user-data"
expect_failure unknown-nonempty-root
[ "$(cat "$target/user-data")" = precious ] || fail 'user data changed'
new_root stale-stage; mkdir -- "$target/tmp" "$target/tmp/bootstrap.old"
expect_failure stale-uninstalled-staging
new_root failed-download; TEST_CURL_FAIL=1; export TEST_CURL_FAIL
expect_failure failed-download
unset TEST_CURL_FAIL
fixture bad-outer; pack
printf '%s\n' corruption >> "$fixture_root/nestbot-v0.1.0-x86_64-linux.tar.gz"
select_fixture bad-outer; new_root bad-outer
expect_failure outer-checksum
fixture bad-inner
printf '%s\n' corruption >> "$source/doc/deployment.md"
pack; select_fixture bad-inner; new_root bad-inner
expect_failure inner-checksum
fixture extra-file
printf '%s\n' bad > "$source/bin/unexpected"
pack; select_fixture extra-file; new_root extra-file
expect_failure unexpected-archive-file
fixture duplicate-member
(cd -- "$source" && tar -czf "$fixture_root/nestbot-v0.1.0-x86_64-linux.tar.gz" bin config scripts doc SHA256SUMS bin/nestbot)
cp -- "$fixture_root/nestbot-v0.1.0-x86_64-linux.tar.gz" "$fixture_root/nestbot-v0.1.0-aarch64-linux.tar.gz"
(cd -- "$fixture_root" && sha256sum --text nestbot-v0.1.0-x86_64-linux.tar.gz nestbot-v0.1.0-aarch64-linux.tar.gz > SHA256SUMS)
select_fixture duplicate-member; new_root duplicate-member
expect_failure duplicate-archive-member
fixture unexpected-directory
mkdir -- "$source/doc/unexpected"
pack; select_fixture unexpected-directory; new_root unexpected-directory
expect_failure unexpected-archive-directory
fixture malicious-manifest
printf '%s\n' '0000000000000000000000000000000000000000000000000000000000000000  ../escape' > "$source/SHA256SUMS"
pack; select_fixture malicious-manifest; new_root malicious-manifest
expect_failure manifest-traversal
fixture symlink
rm -- "$source/doc/deployment.md"
# GNU tar transforms a normal member into an explicit symlink fixture using ln.
# Git Bash installations without native symlinks cannot faithfully run this case.
if ln -s ../bin/nestbot "$source/doc/deployment.md" && [ -L "$source/doc/deployment.md" ]; then
    pack; select_fixture symlink; new_root symlink
    expect_failure archive-symlink
else
    printf '%s\n' 'SKIP symlink archive fixture: host cannot create POSIX symlinks'
fi
fixture hardlink
rm -- "$source/doc/deployment.md"
ln -- "$source/bin/nestbot" "$source/doc/deployment.md"
pack; select_fixture hardlink; new_root hardlink
expect_failure archive-hardlink
fixture fifo
rm -- "$source/doc/deployment.md"
if command -v mkfifo >/dev/null 2>&1 && mkfifo "$source/doc/deployment.md" 2>/dev/null; then
    pack; select_fixture fifo; new_root fifo
    expect_failure archive-special-file
else
    printf '%s\n' 'SKIP FIFO archive fixture: host cannot create POSIX FIFOs'
fi
fixture traversal
(cd -- "$source" && tar --transform='s|^bin/nestbot$|../escape|' -czf "$fixture_root/nestbot-v0.1.0-x86_64-linux.tar.gz" bin config scripts doc SHA256SUMS)
cp -- "$fixture_root/nestbot-v0.1.0-x86_64-linux.tar.gz" "$fixture_root/nestbot-v0.1.0-aarch64-linux.tar.gz"
(cd -- "$fixture_root" && sha256sum --text nestbot-v0.1.0-x86_64-linux.tar.gz nestbot-v0.1.0-aarch64-linux.tar.gz > SHA256SUMS)
select_fixture traversal; new_root traversal
expect_failure archive-traversal
fixture absolute
(cd -- "$source" && tar --transform='s|^bin/nestbot$|/escape|' -czf "$fixture_root/nestbot-v0.1.0-x86_64-linux.tar.gz" bin config scripts doc SHA256SUMS)
cp -- "$fixture_root/nestbot-v0.1.0-x86_64-linux.tar.gz" "$fixture_root/nestbot-v0.1.0-aarch64-linux.tar.gz"
(cd -- "$fixture_root" && sha256sum --text nestbot-v0.1.0-x86_64-linux.tar.gz nestbot-v0.1.0-aarch64-linux.tar.gz > SHA256SUMS)
select_fixture absolute; new_root absolute
expect_failure archive-absolute-path
# Every fake download must stay below its explicitly selected deployment root.
while IFS= read -r download; do
    case "$download" in "$work/roots/"*/tmp/bootstrap.*/archive/*|"$work/roots/"*/tmp/bootstrap.*/checksums/SHA256SUMS) ;;
        *) fail "download escaped application root: $download" ;;
    esac
done < "$TEST_DOWNLOAD_LOG"
printf '%s\n' 'PASS download confinement' 'Offline bootstrap smoke tests passed; real Linux deployment remains a separate acceptance test.'
