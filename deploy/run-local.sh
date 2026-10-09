#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
cd "$ROOT"
umask 077
mkdir -p .local/tmp .local/log .local/run
export TMPDIR="$ROOT/.local/tmp"
export SQLITE_TMPDIR="$TMPDIR"
ulimit -c 0
PID_FILE="$ROOT/.local/run/service.pid"
LOG_FILE="$ROOT/.local/log/nestbot.log"

fail() { printf '错误：%s\n' "$*" >&2; exit 1; }
cli() { "$ROOT/nestbot" --config "$ROOT/config/nestbot.toml" --env-file "$ROOT/config/secrets.env" "$@"; }
read_pid() {
    [ -f "$PID_FILE" ] || return 1
    IFS= read -r PID < "$PID_FILE" || return 1
    case "$PID" in ''|*[!0-9]*|0|1) return 1;; esac
}
alive() {
    kill -0 "$1" 2>/dev/null || return 1
    state=$(ps -p "$1" -o stat= 2>/dev/null) || return 1
    case "$state" in *Z*) return 1;; esac
}
owned() {
    args=$(ps -ww -p "$1" -o args= 2>/dev/null) || return 1
    [ "$args" = "$ROOT/nestbot --config $ROOT/config/nestbot.toml --env-file $ROOT/config/secrets.env serve" ]
}
start_service() {
    if cli status >/dev/null 2>&1; then
        printf '服务已经运行，未重复启动。\n'
        return
    fi
    if read_pid && alive "$PID"; then
        owned "$PID" || fail "PID 文件指向其他进程，拒绝启动或操作它：$PID_FILE"
        fail "服务进程仍在运行但控制接口未就绪；查看日志：$LOG_FILE"
    fi
    rm -f "$PID_FILE"
    nohup "$ROOT/deploy/run-local.sh" serve >> "$LOG_FILE" 2>&1 < /dev/null &
    PID=$!
    printf '%s\n' "$PID" > "$PID_FILE"
    attempt=0
    while [ "$attempt" -lt 30 ]; do
        if ! alive "$PID"; then
            rm -f "$PID_FILE"
            fail "启动失败，查看日志：$LOG_FILE"
        fi
        if cli status >/dev/null 2>&1; then
            printf '启动成功，PID=%s\n日志：%s\n' "$PID" "$LOG_FILE"
            return
        fi
        attempt=$((attempt + 1))
        sleep 1
    done
    # We created this process, but still verify ownership before signalling it.
    if owned "$PID"; then kill -TERM "$PID" 2>/dev/null || :; fi
    fail "服务未能就绪，已请求停止；查看日志：$LOG_FILE"
}
shutdown_service() {
    if ! read_pid; then
        if cli status >/dev/null 2>&1; then
            fail '服务运行中但没有可靠的 PID 记录；不能安全停止。请停止之前手工启动的 serve 进程。'
        fi
        printf '服务未运行。\n'
        return
    fi
    if ! alive "$PID"; then
        rm -f "$PID_FILE"
        printf '服务未运行，已清理过期 PID 文件。\n'
        return
    fi
    owned "$PID" || fail "PID 已被其他进程复用，拒绝发送信号：$PID"
    kill -TERM "$PID"
    attempt=0
    while alive "$PID" && [ "$attempt" -lt 30 ]; do
        sleep 1
        attempt=$((attempt + 1))
    done
    alive "$PID" && fail '服务尚未退出，未强制终止；请查看日志后处理。'
    rm -f "$PID_FILE"
    printf '服务已停止。\n'
}

case "${1:-}" in
    start|shutdown|restart)
        [ "$#" -eq 1 ] || fail 'start、shutdown、restart 不接受其他参数。'
        command -v ps >/dev/null 2>&1 || fail '缺少 ps 命令。'
        mkdir .local/run/lifecycle.lock 2>/dev/null || fail '另一个启动/停止操作正在进行；异常中断留下的 lifecycle.lock 请确认无其他操作后删除。'
        trap 'rmdir "$ROOT/.local/run/lifecycle.lock" 2>/dev/null || :' 0
        trap 'exit 1' HUP INT TERM
        case "$1" in
            start) start_service;;
            shutdown) shutdown_service;;
            restart) shutdown_service; start_service;;
        esac
        ;;
    *) exec "$ROOT/nestbot" --config "$ROOT/config/nestbot.toml" --env-file "$ROOT/config/secrets.env" "$@";;
esac
