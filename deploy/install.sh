#!/bin/sh
set -eu

fail() { printf '部署失败：%s\n' "$*" >&2; exit 1; }
usage() { printf '%s\n' 'NestBot 非交互部署及 systemd 开机自启' '用法：sh install.sh [--dir 安装目录]' '默认目录：$HOME/nestbot；不询问凭据，不登录账号，不覆盖已有配置。'; }
cleanup() {
    if [ -n "${STAGE:-}" ]; then rm -rf "$STAGE"; fi
    if [ -n "${INSTALL_LOCK:-}" ]; then rmdir "$INSTALL_LOCK" 2>/dev/null || :; fi
}
privileged() {
    if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo -n "$@"; fi
}
unit_path() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/%/%%/g'; }
show_commands() {
    printf '\n部署目录：%s\n' "$ROOT"
    printf '编辑配置：nano "%s/config/nestbot.toml"\n' "$ROOT"
    printf '编辑凭据及密码：nano "%s/config/secrets.env"\n' "$ROOT"
    printf '首次初始化："%s/deploy/run-local.sh" init\n' "$ROOT"
    printf '登录主账号："%s/deploy/run-local.sh" login\n' "$ROOT"
    printf '登录第二账号："%s/deploy/run-local.sh" login --upload\n' "$ROOT"
    printf '配置登录完成后启动："%s/deploy/run-local.sh" start\n' "$ROOT"
    printf '状态："%s/deploy/run-local.sh" status\n' "$ROOT"
    printf '停止："%s/deploy/run-local.sh" shutdown\n' "$ROOT"
    printf '日志：tail -n 100 "%s/.local/log/nestbot.log"\n' "$ROOT"
}
main() {
    INSTALL_DIR="${HOME:?HOME 未设置}/nestbot"
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --dir) [ "$#" -ge 2 ] || fail '--dir 后面需要目录路径。'; INSTALL_DIR=$2; shift 2;;
            --help|-h) usage; return;;
            *) fail "不认识的参数：$1";;
        esac
    done
    [ "$(uname -s)" = Linux ] || fail '仅支持 Linux。'
    case "$(uname -m)" in
        x86_64|amd64) ARCH=x86_64;;
        aarch64|arm64) ARCH=aarch64;;
        *) fail '仅支持 64 位 x86_64 和 aarch64 Linux。';;
    esac
    for tool in curl tar gzip sha256sum mktemp systemctl install sed; do
        command -v "$tool" >/dev/null 2>&1 || fail "缺少 $tool，请先用系统包管理器安装。"
    done
    [ -d /run/systemd/system ] || fail '系统必须以 systemd 运行；不支持未启用 systemd 的容器。'
    if [ "$(id -u)" -ne 0 ]; then
        command -v sudo >/dev/null 2>&1 || fail '注册系统服务需要 root 权限或 sudo。'
        sudo -n true 2>/dev/null || fail '请先单独执行 sudo -v，再重新运行部署命令；安装脚本不交互询问密码。'
    fi
    case "$INSTALL_DIR" in ''|/) fail '安装目录不能为空或是根目录 /。';; esac
    umask 077
    mkdir -p "$INSTALL_DIR"
    ROOT=$(CDPATH= cd -- "$INSTALL_DIR" && pwd -P)
    [ "$ROOT" != / ] || fail '安装目录不能解析为根目录 /。'
    case "$ROOT" in *'
'*|*"
"*) fail '安装目录不能包含换行或回车。';; esac
    cd "$ROOT"
    existing=$(systemctl show nestbot.service --property=WorkingDirectory --value 2>/dev/null || :)
    [ -z "$existing" ] || [ "$existing" = "$ROOT" ] || fail "已有 nestbot.service 属于其他目录：$existing，拒绝覆盖。"
    mkdir -p .local/tmp .local/log .local/run config
    trap cleanup 0
    trap 'exit 1' HUP INT TERM
    mkdir .local/run/install.lock 2>/dev/null || fail '另一个部署正在进行；异常中断的 install.lock 请确认无其他部署后删除。'
    INSTALL_LOCK="$ROOT/.local/run/install.lock"
    STAGE=$(mktemp -d "$ROOT/.local/tmp/install.XXXXXX")
    ASSET="nestbot-linux-$ARCH.tar.gz"
    BASE='https://github.com/0pT1ck/NestBot/releases/latest/download'
    printf '下载及校验 %s，安装目录：%s\n' "$ASSET" "$ROOT"
    curl -fsSL "$BASE/$ASSET" -o "$STAGE/$ASSET"
    curl -fsSL "$BASE/$ASSET.sha256" -o "$STAGE/$ASSET.sha256"
    IFS=' ' read -r digest checked_file < "$STAGE/$ASSET.sha256" || fail '校验文件损坏。'
    [ "${#digest}" -eq 64 ] || fail 'SHA-256 长度不正确。'
    case "$digest" in *[!a-fA-F0-9]*) fail 'SHA-256 格式不正确。';; esac
    [ "$checked_file" = "$ASSET" ] || fail '校验文件对应错误的程序包。'
    (cd "$STAGE" && printf '%s  %s\n' "$digest" "$ASSET" | sha256sum -c -) || fail '程序包校验失败，原服务和文件未改动。'
    mkdir "$STAGE/unpacked"
    tar -xzf "$STAGE/$ASSET" -C "$STAGE/unpacked"
    for file in nestbot deploy/run-local.sh deploy/package-version config/nestbot.example.toml config/secrets.example.env; do
        [ -f "$STAGE/unpacked/$file" ] || fail "发布包缺少 $file。"
    done
    IFS= read -r package_version < "$STAGE/unpacked/deploy/package-version" || fail '发布包版本信息损坏。'
    [ "$package_version" = systemd-1 ] || fail '公开 Release 与此安装入口不兼容，请等待新版发布；原服务及配置未改动。'
    # Stop only after the complete package has passed verification.
    if [ -n "$existing" ]; then privileged systemctl stop nestbot.service; fi
    # One-time cutover from the previous nohup launcher; it checks PID ownership.
    if [ -f .local/run/service.pid ]; then
        [ -x deploy/run-local.sh ] || fail '旧后台进程缺少管理入口，请先手工停止旧服务。'
        deploy/run-local.sh shutdown
        rm -f .local/run/service.pid
    fi
    cp "$STAGE/unpacked/nestbot" "$ROOT/nestbot"
    cp "$STAGE/unpacked/README.md" "$ROOT/README.md"
    mkdir -p deploy docs
    cp -R "$STAGE/unpacked/deploy/." "$ROOT/deploy/"
    cp -R "$STAGE/unpacked/docs/." "$ROOT/docs/"
    cp "$STAGE/unpacked/config/nestbot.example.toml" config/nestbot.example.toml
    cp "$STAGE/unpacked/config/secrets.example.env" config/secrets.example.env
    if [ -f .local/data/master.key ]; then
        [ -f config/nestbot.toml ] && [ -f config/secrets.env ] || fail '已有加密数据但配置不完整，请恢复原配置及原密码；不会重置。'
    else
        [ -e config/nestbot.toml ] || cp config/nestbot.example.toml config/nestbot.toml
        [ -e config/secrets.env ] || cp config/secrets.example.env config/secrets.env
    fi
    chmod 600 config/nestbot.toml config/secrets.env
    chmod +x nestbot deploy/install.sh deploy/run-local.sh
    touch .local/log/nestbot.log
    escaped=$(unit_path "$ROOT")
    # Single-path directives do not strip argument quotes; only escape specifiers.
    raw_path=$(printf '%s' "$ROOT" | sed 's/%/%%/g')
    cat > "$STAGE/nestbot.service" <<EOF
[Unit]
Description=NestBot Telegram service
Wants=network-online.target
After=network-online.target
ConditionPathExists=$raw_path/.local/data/master.key

[Service]
Type=exec
User=$(id -u)
WorkingDirectory=$raw_path
ExecStartPre="$escaped/deploy/run-local.sh" doctor
ExecStart="$escaped/deploy/run-local.sh" serve
Restart=on-failure
RestartSec=5
TimeoutStopSec=45
UMask=0077
LimitCORE=0
CPUQuota=50%
MemoryHigh=384M
MemoryMax=512M
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths="$escaped"
StandardOutput=append:$raw_path/.local/log/nestbot.log
StandardError=append:$raw_path/.local/log/nestbot.log

[Install]
WantedBy=multi-user.target
EOF
    # Root-owned copy, not a symlink to a user-writable unit: prevents privilege escalation.
    privileged install -m 644 "$STAGE/nestbot.service" /etc/systemd/system/nestbot.service
    privileged systemctl daemon-reload
    privileged systemctl enable nestbot.service
    if [ -f .local/data/master.key ]; then
        "$ROOT/deploy/run-local.sh" start
        printf '已有配置：服务已后台启动，并启用开机自启。\n'
    else
        printf '部署完成，开机自启已注册。尚未初始化：请编辑配置、初始化、登录后再启动。\n'
    fi
    show_commands
}
main "$@"
