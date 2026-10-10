#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$ROOT"
umask 077
mkdir -p .local/tmp .local/log .local/run
export TMPDIR="$ROOT/.local/tmp"
export SQLITE_TMPDIR="$TMPDIR"
ulimit -c 0

fail() { printf '错误：%s\n' "$*" >&2; exit 1; }
cli() { "$ROOT/nestbot" --config "$ROOT/config/nestbot.toml" --env-file "$ROOT/config/secrets.env" "$@"; }
service_command() {
    actual=$(systemctl show nestbot.service --property=WorkingDirectory --value 2>/dev/null) || fail '未找到 systemd 服务，请先执行安装脚本。'
    [ "$actual" = "$ROOT" ] || fail 'nestbot.service 不属于此安装目录，拒绝操作。'
    if [ "$(id -u)" -eq 0 ]; then systemctl "$1" nestbot.service; else sudo systemctl "$1" nestbot.service; fi
}
case "${1:-}" in
    start|shutdown|restart)
        [ "$#" -eq 1 ] || fail 'start、shutdown、restart 不接受其他参数。'
        if [ "$1" = shutdown ]; then
            service_command stop
            printf '服务已停止，开机自启设置保留。\n'
        else
            [ -f .local/data/master.key ] || fail '请先编辑 config/ 中的配置和密码，再执行 init、login，最后 start。'
            cli doctor >/dev/null
            service_command "$1"
            attempt=0
            while [ "$attempt" -lt 30 ]; do
                if cli status >/dev/null 2>&1; then
                    printf '服务已就绪；开机自启由 systemd 管理。\n'
                    exit 0
                fi
                if ! systemctl is-active --quiet nestbot.service; then
                    fail "服务未运行，请查看 $ROOT/.local/log/nestbot.log 和 systemctl status nestbot。"
                fi
                attempt=$((attempt + 1))
                sleep 1
            done
            fail "进程存在但控制接口未就绪，请查看 $ROOT/.local/log/nestbot.log。"
        fi
        ;;
    *) exec "$ROOT/nestbot" --config "$ROOT/config/nestbot.toml" --env-file "$ROOT/config/secrets.env" "$@";;
esac
