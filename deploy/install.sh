#!/bin/sh
set -eu

fail() { printf '安装失败：%s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' 'NestBot 单目录安装并启动' '用法：sh install.sh [--dir 安装目录] [--no-login]' '默认目录：$HOME/nestbot；--no-login 仅在已有配置时跳过账号登录。'
}
cleanup() {
    if [ -n "${TTY_STATE:-}" ]; then stty "$TTY_STATE" < /dev/tty 2>/dev/null || :; fi
    if [ -n "${STAGE:-}" ]; then rm -rf "$STAGE"; fi
    if [ -n "${INSTALL_LOCK:-}" ]; then rmdir "$INSTALL_LOCK" 2>/dev/null || :; fi
}
prompt() {
    printf '%s' "$1" >&2
    IFS= read -r REPLY < /dev/tty || fail '未读取到输入。请在 SSH 终端里执行安装命令。'
    printf '%s' "$REPLY"
}
secret() {
    printf '%s' "$1" >&2
    stty -echo < /dev/tty
    IFS= read -r REPLY < /dev/tty || fail '未读取到输入。'
    stty "$TTY_STATE" < /dev/tty
    printf '\n' >&2
    printf '%s' "$REPLY"
}
positive_number() {
    case "$1" in ''|0*|*[!0-9]*) fail "$2 必须是正整数，不是用户名或手机号。";; esac
}
bot_name() {
    case "$1" in ''|*[!a-zA-Z0-9_]*) fail '机器人用户名只填写名称，例如 example_bot，不要填写 t.me 链接。';; esac
}
password() {
    PASSWORD=$(secret "$1")
    [ "${#PASSWORD}" -ge 12 ] || fail '密码至少需要 12 个字符。'
    CONFIRM=$(secret '再输入一次确认：')
    [ "$PASSWORD" = "$CONFIRM" ] || fail '两次密码不一致，请重新运行安装命令。'
    # The application trims env-file values; do not silently alter a password.
    case "$PASSWORD" in [[:space:]]*|*[[:space:]]) fail '密码开头和结尾不能有空白。';; esac
}
show_commands() {
    printf '\n安装目录：%s\n' "$ROOT"
    printf '以后启动："%s/deploy/run-local.sh" start\n' "$ROOT"
    printf '查看状态："%s/deploy/run-local.sh" status\n' "$ROOT"
    printf '停止服务："%s/deploy/run-local.sh" shutdown\n' "$ROOT"
    printf '重启服务："%s/deploy/run-local.sh" restart\n' "$ROOT"
    printf '查看日志：tail -n 50 "%s/.local/log/nestbot.log"\n' "$ROOT"
    printf 'Web 仅监听服务器本机；不要为方便而直接向公网暴露管理端口。\n'
}
configure() {
    if [ -f "$ROOT/config/nestbot.toml" ] && [ -f "$ROOT/config/secrets.env" ]; then
        printf '保留已有配置、密码、账号会话和数据库。\n'
        return
    fi
    [ ! -f "$ROOT/.local/data/master.key" ] || fail '已有加密数据但配置不完整，请恢复原配置和原数据库密码；不会重置密码。'
    [ ! -e "$ROOT/config/nestbot.toml" ] && [ ! -e "$ROOT/config/secrets.env" ] || fail '只找到部分配置；请补齐原配置，不会自动覆盖它。'
    [ "$NO_LOGIN" -eq 0 ] || fail '--no-login 需要已有 config/nestbot.toml 和 config/secrets.env。'
    command -v stty >/dev/null 2>&1 || fail '交互配置需要 stty 命令。'
    TTY_STATE=$(stty -g < /dev/tty) || fail '需要可交互的终端；请通过 SSH 登录后执行命令。'
    printf '\n首次配置：按提示填写。密码输入时不会显示，这是正常现象。\n'
    API_ID=$(prompt 'Telegram API ID（my.telegram.org 中的数字）：')
    positive_number "$API_ID" 'API ID'
    [ "${#API_ID}" -le 10 ] && [ "$API_ID" -le 2147483647 ] || fail 'API ID 超出有效范围。'
    API_HASH=$(secret 'Telegram API Hash：')
    case "$API_HASH" in ''|*[!a-fA-F0-9]*) fail 'API Hash 应是 my.telegram.org 提供的十六进制字符串。';; esac
    [ "${#API_HASH}" -eq 32 ] || fail 'API Hash 应为 32 个字符。'
    BOT_TOKEN=$(secret '你的 Telegram Bot Token（从 @BotFather 获取）：')
    case "$BOT_TOKEN" in ''|*[!a-zA-Z0-9_:-]*) fail 'Bot Token 格式不正确。';; esac
    case "$BOT_TOKEN" in *:*) :;; *) fail 'Bot Token 应含冒号，不是 API Hash。';; esac
    OWNER=$(prompt '允许使用你的 Bot 的 Telegram 数字用户 ID：')
    positive_number "$OWNER" '用户 ID'
    [ "${#OWNER}" -le 19 ] && [ "$OWNER" -le 9223372036854775807 ] || fail '用户 ID 超出有效范围。'
    SEARCH_BOT=$(prompt '搜索机器人的用户名（不填 @）：')
    SEARCH_BOT=${SEARCH_BOT#@}
    bot_name "$SEARCH_BOT"
    FILE_BOT=$(prompt '提取机器人的用户名（不填 @）：')
    FILE_BOT=${FILE_BOT#@}
    bot_name "$FILE_BOT"
    TARGET=$(prompt '默认转存目标（例如 -1001234567890 或 @my_channel；回车表示稍后 /bind）：')
    case "$TARGET" in *[!a-zA-Z0-9_@-]*) fail '目标请填写聊天数字 ID 或公开用户名，不要填链接或引号。';; esac
    PROXY=$(secret '可选 SOCKS5 代理 URL（不需要就直接回车）：')
    case "$PROXY" in ''|socks5://*) :;; *) fail '代理仅支持 socks5:// URL。';; esac
    password '数据库解锁密码（至少 12 字符，初始化后不要更改）：'
    VAULT_PASSWORD=$PASSWORD
    password 'Web 管理密码（至少 12 字符，建议不同于数据库密码）：'
    ADMIN_PASSWORD=$PASSWORD
    printf '%s\n' '请安全保存数据库密码；丢失它或 master.key 后无法恢复数据。'
    cat > "$STAGE/nestbot.toml" <<EOF
# 业务文件全部位于安装目录内；不要修改为系统目录。
default_mode = "copy"
default_target = "$TARGET"
[paths]
data = ".local/data"
cache = ".local/cache"
run = ".local/run"
[telegram]
api_id = $API_ID
allowed_users = [$OWNER]
search_bot = "$SEARCH_BOT"
file_bot = "$FILE_BOT"
[web]
enabled = true
listen = "127.0.0.1:8787"
EOF
    {
        printf 'VAULT_PASSWORD="%s"\n' "$VAULT_PASSWORD"
        printf 'NESTBOT_ADMIN_PASSWORD="%s"\n' "$ADMIN_PASSWORD"
        printf 'TELEGRAM_API_HASH="%s"\n' "$API_HASH"
        printf 'TELEGRAM_BOT_TOKEN="%s"\n' "$BOT_TOKEN"
        if [ -n "$PROXY" ]; then printf 'NESTBOT_PROXY="%s"\n' "$PROXY"; fi
    } > "$STAGE/secrets.env"
    # Validate before installing any user configuration.
    "$ROOT/nestbot" --config "$STAGE/nestbot.toml" --env-file "$STAGE/secrets.env" doctor >/dev/null
    mv "$STAGE/nestbot.toml" "$ROOT/config/nestbot.toml"
    mv "$STAGE/secrets.env" "$ROOT/config/secrets.env"
    chmod 600 "$ROOT/config/nestbot.toml" "$ROOT/config/secrets.env"
    unset API_HASH BOT_TOKEN PROXY VAULT_PASSWORD ADMIN_PASSWORD PASSWORD CONFIRM REPLY
}
main() {
    INSTALL_DIR="${HOME:?HOME 未设置}/nestbot"
    NO_LOGIN=0
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --dir) [ "$#" -ge 2 ] || fail '--dir 后面需要目录路径。'; INSTALL_DIR=$2; shift 2;;
            --no-login) NO_LOGIN=1; shift;;
            --help|-h) usage; return;;
            *) fail "不认识的参数：$1";;
        esac
    done
    [ "$(uname -s)" = Linux ] || fail '此安装入口只支持 Linux，不支持 macOS 或 Windows。'
    case "$(uname -m)" in
        x86_64|amd64) ARCH=x86_64;;
        aarch64|arm64) ARCH=aarch64;;
        *) fail '仅支持 64 位 x86_64 和 aarch64 Linux。';;
    esac
    for tool in curl tar gzip sha256sum mktemp nohup ps; do
        command -v "$tool" >/dev/null 2>&1 || fail "缺少命令 $tool；请先用系统包管理器安装对应工具，脚本不会替你修改系统。"
    done
    case "$INSTALL_DIR" in ''|/) fail '安装目录不能为空或是根目录 /。';; esac
    umask 077
    mkdir -p "$INSTALL_DIR"
    ROOT=$(CDPATH= cd -- "$INSTALL_DIR" && pwd -P)
    [ "$ROOT" != / ] || fail '安装目录不能解析为根目录 /。'
    cd "$ROOT"
    if [ -x deploy/run-local.sh ] && [ -x nestbot ] && deploy/run-local.sh status >/dev/null 2>&1; then
        printf '现有服务已运行；未替换运行中的程序，也未覆盖配置。\n'
        show_commands
        return
    fi
    if [ -f .local/run/service.pid ]; then
        IFS= read -r old_pid < .local/run/service.pid || old_pid=''
        case "$old_pid" in ''|*[!0-9]*) :;; *)
            if kill -0 "$old_pid" 2>/dev/null; then fail '记录的进程仍在运行。请先执行 deploy/run-local.sh shutdown，再重新安装/更新。'; fi;;
        esac
    fi
    mkdir -p .local/tmp .local/log .local/run config
    TTY_STATE=''
    trap cleanup 0
    trap 'exit 1' HUP INT TERM
    mkdir .local/run/install.lock 2>/dev/null || fail '另一个安装操作正在进行；异常中断留下的 install.lock 请确认无其他安装操作后删除。'
    INSTALL_LOCK="$ROOT/.local/run/install.lock"
    STAGE=$(mktemp -d "$ROOT/.local/tmp/install.XXXXXX")
    ASSET="nestbot-linux-$ARCH.tar.gz"
    BASE='https://github.com/0pT1ck/NestBot/releases/latest/download'
    printf '安装目录：%s\n下载 %s 的预编译程序，不在 VPS 编译。\n' "$ROOT" "$ARCH"
    curl --fail --location --silent --show-error "$BASE/$ASSET" --output "$STAGE/$ASSET" || fail '无法下载公开 Release；请检查 GitHub 网络连接或 Release 是否已发布。'
    curl --fail --location --silent --show-error "$BASE/$ASSET.sha256" --output "$STAGE/$ASSET.sha256" || fail '无法下载校验文件，未安装程序。'
    # Accept exactly this file's checksum, not arbitrary paths from a checksum file.
    IFS=' ' read -r digest checked_file < "$STAGE/$ASSET.sha256" || fail '校验文件损坏。'
    [ "${#digest}" -eq 64 ] || fail 'SHA-256 校验值长度不正确。'
    case "$digest" in *[!a-fA-F0-9]*) fail 'SHA-256 校验值不正确。';; esac
    [ "$checked_file" = "$ASSET" ] || fail '校验文件对应了错误的程序包。'
    (cd "$STAGE" && printf '%s  %s\n' "$digest" "$ASSET" | sha256sum -c -) || fail '程序包校验失败，未安装程序。'
    mkdir "$STAGE/unpacked"
    tar -xzf "$STAGE/$ASSET" -C "$STAGE/unpacked"
    [ -f "$STAGE/unpacked/nestbot" ] && [ -f "$STAGE/unpacked/deploy/run-local.sh" ] || fail '发布包不完整。'
    # Copy only release files; never extract directly over real config/data.
    for file in nestbot README.md; do
        [ ! -f "$STAGE/unpacked/$file" ] || cp "$STAGE/unpacked/$file" "$ROOT/$file"
    done
    mkdir -p deploy docs
    cp -R "$STAGE/unpacked/deploy/." "$ROOT/deploy/"
    cp -R "$STAGE/unpacked/docs/." "$ROOT/docs/"
    cp "$STAGE/unpacked/config/nestbot.example.toml" "$ROOT/config/nestbot.example.toml"
    cp "$STAGE/unpacked/config/secrets.example.env" "$ROOT/config/secrets.example.env"
    chmod +x "$ROOT/nestbot" "$ROOT/deploy/install.sh" "$ROOT/deploy/run-local.sh"
    configure
    "$ROOT/deploy/run-local.sh" doctor
    "$ROOT/deploy/run-local.sh" init
    if [ "$NO_LOGIN" -eq 0 ]; then
        printf '\n登录第一个提取账号，请输入自己的手机号及 Telegram 验证码。\n'
        "$ROOT/deploy/run-local.sh" login < /dev/tty
        SECOND=$(prompt '是否登录第二个账号，用于受限后轮换？[Y/n]：')
        case "$SECOND" in n|N|no|NO) :;; *) "$ROOT/deploy/run-local.sh" login --upload < /dev/tty;; esac
    fi
    "$ROOT/deploy/run-local.sh" start
    show_commands
}
main "$@"
