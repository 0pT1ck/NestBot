# Linux 部署：安装、配置、登录分开

## 1. 一条命令部署

需要以 systemd 启动的 64 位 Linux（推荐 Debian 12/13、Ubuntu 22.04/24.04；x86_64 或 ARM64）。不在小 VPS 编译，不需要 Rust、Python、Docker 或额外数据库服务。

在 VPS 的 SSH 终端执行：

```sh
curl -fsSL https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/install.sh | sh
```

默认 `$HOME/nestbot`：root 用户为 `/root/nestbot`，普通用户通常为 `/home/用户名/nestbot`。指定路径：

```sh
curl -fsSL https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/install.sh | sh -s -- --dir /xxx/nestbot
```

`/xxx/nestbot` 要换成你拥有写权限的目录，优先用默认短路径，避免超过 Unix socket 长度限制。脚本不会询问 API、密码、手机号、验证码，也不读取交互终端。

注册系统服务需要 root 权限。root 直接执行；普通用户先单独执行 `sudo -v`，完成系统的 sudo 验证后再执行安装命令。脚本使用 `sudo -n`，没有可用权限时明确退出，不在 curl 管道里询问密码。程序以执行安装的用户身份运行；配置和后续登录也使用该用户，不要混用 `sudo` 改变数据目录所有者。

缺少 curl 的 Debian/Ubuntu：

```sh
sudo apt update && sudo apt install -y curl ca-certificates
```

root 没有 sudo 时去掉 sudo。精简系统还需 `tar`、`gzip`、`sha256sum`、`mktemp`、`sed`、`systemctl` 等常见系统命令；文件编辑示例使用 `nano`，缺少时通过包管理器安装。

部署会下载静态程序并校验 SHA-256，生成可编辑配置文件，注册后台服务并开启开机自启。已有配置和数据不覆盖；更新在下载及校验完成后停止原服务，再替换程序。旧 nohup 后台实例会先通过原入口安全停止，再切换到 systemd。

**首次安装不会用示例密码初始化数据库。** 新密码字段为空，没有主密钥时服务等待你完成下述配置、初始化和登录；看到“开机自启已注册”并不代表 Telegram 已配置好。已有主密钥及有效配置的安装会自动后台启动。

## 2. 直接编辑两个配置文件

以下命令以默认安装目录为例；自选目录将 `~/nestbot` 换成自己的路径。

```sh
nano ~/nestbot/config/nestbot.toml
```

修改下面这些字段，保留字段名和 TOML 语法：

| 字段 | 填什么 |
| --- | --- |
| `default_target` | `"-100..."` 频道 ID 或 `"@公开频道用户名"`；可先留 `""`，之后通过 `/bind` 设置 |
| `default_mode` | 建议 `"copy"`，避免不必要的下载上传 |
| `[telegram]` 的 `api_id` | my.telegram.org → API development tools 中的数字 API ID，不加引号 |
| `allowed_users` | 你的 Telegram 数字用户 ID，例如 `[123456789]`，不是手机号或用户名 |
| `search_bot` | 实际搜索机器人用户名，放在双引号内，不填 t.me 链接 |
| `file_bot` | 实际文件机器人用户名，放在双引号内，不填 t.me 链接 |

`[paths]` 保留 `.local/data`、`.local/cache`、`.local/run`，不用改为系统目录。Web 默认 `127.0.0.1:8787`，不要直接暴露公网。

再编辑凭据文件：

```sh
nano ~/nestbot/config/secrets.env
```

| 字段 | 填什么 |
| --- | --- |
| `VAULT_PASSWORD` | 自己设置的数据库解锁密码，至少 12 字符，必须安全保存；已有数据库时必须保留原密码 |
| `NESTBOT_ADMIN_PASSWORD` | Web 管理密码，至少 12 字符，建议不同于数据库密码 |
| `TELEGRAM_API_HASH` | 与 API ID 同一页面提供的 API Hash |
| `TELEGRAM_BOT_TOKEN` | 从官方 @BotFather 获取的管理机器人 Token |
| `NESTBOT_PROXY` | 可选 SOCKS5 URL；VPS 可直连 Telegram 就保留注释 |

例如密码可以写成 `VAULT_PASSWORD="你设置的长密码"`。不要照抄示例文字作为真实密码。凭据文件按文本加载，不是 shell 脚本，**不要执行 `source secrets.env`**。注释单独放一行，不在值后面写注释；进程环境变量仍有更高优先级。

nano 编辑完成：按 **Ctrl+O** 保存 → **回车**确认文件名 → **Ctrl+X** 退出。改配置只用编辑器，不再由安装脚本逐项询问。

**两个账号共用同一组 API ID/API Hash，第二个账号不用另申请 App ID。** 管理 Bot 要加入目标频道并授予必要权限；账号也需要相应目标权限。

## 3. 单独初始化和登录账号

首次配置完成后依次执行：

```sh
# 检查配置，不连接 Telegram
~/nestbot/deploy/run-local.sh doctor
# 初始化加密数据库；已有数据库不会重置
~/nestbot/deploy/run-local.sh init
# 登录第一个账号
~/nestbot/deploy/run-local.sh login
# 可选：登录不同的第二个账号
~/nestbot/deploy/run-local.sh login --upload
```

只有登录命令会询问手机号、验证码及必要的二步验证密码。手机号包含国家区号，例如 `+86...`；验证码可能发到 Telegram App，不一定是短信。输入支持 Ctrl-H 和 DEL 两种退格编码；手机号可见，验证码/二步验证密码按程序提示隐藏。不要把验证码或会话发给别人。

第二账号复用第二/上传会话，只在提取受限时主 → 第二 → 主轮换，搜索与上传选择不变。没有第二账号则保留单账号等待。

已经运行的服务必须先停止再登录，避免同时打开同一份账号会话：

```sh
~/nestbot/deploy/run-local.sh shutdown
~/nestbot/deploy/run-local.sh login --upload
~/nestbot/deploy/run-local.sh start
```

不要直接修改已初始化数据库的 `VAULT_PASSWORD`；当前没有密码更换命令，错误修改会无法解锁主密钥。丢失密码或 `master.key` 无法恢复。

## 4. 后台运行与开机自启

配置和登录完成后：

```sh
~/nestbot/deploy/run-local.sh start
~/nestbot/deploy/run-local.sh status
```

`start` 检查配置并等待本地控制接口就绪；成功后可以关闭 SSH。systemd 会在 VPS 开机时启动，也会在异常退出后重启。不需要 screen、tmux 或 nohup。

```sh
# 停止整个服务（仍保留开机自启）
~/nestbot/deploy/run-local.sh shutdown
# 修改配置后重启
~/nestbot/deploy/run-local.sh restart
# 查看应用日志
 tail -n 100 ~/nestbot/.local/log/nestbot.log
# 查看系统服务状态与自启注册
systemctl status nestbot
systemctl is-enabled nestbot
```

`stop` 和 Telegram `/stop` 只停止任务，不关闭服务。永久关闭开机自启使用 `sudo systemctl disable --now nestbot`；root 去掉 sudo。重新开启使用 `sudo systemctl enable nestbot`，然后通过入口 `start`。不要另外启动 `serve` 常驻实例，与 systemd 同时运行会争用同一份数据库。

后台服务设置 CPUQuota=50%、MemoryHigh=384M、MemoryMax=512M，日志仍不会自动轮转；需定期归档。512MiB 是 cgroup 硬上限，不代表真实 Telegram 转存峰值已测定；超限可能被 OOM 终止并重启，需观察实际 VPS。

## 5. 文件位置与系统目录例外

| 安装目录内 | 内容 |
| --- | --- |
| `nestbot`、`deploy/`、`docs/` | 程序、入口、说明 |
| `config/` | TOML 配置及凭据文件，真实配置权限 600 |
| `.local/data/` | SQLite/WAL、加密账号会话、主密钥 `master.key` |
| `.local/cache/` | deep 转存的临时媒体 |
| `.local/run/` | 控制 socket、服务发现、部署操作锁 |
| `.local/tmp/` | 程序/SQLite 临时文件及下载暂存；部署结束清理下载包 |
| `.local/log/nestbot.log` | 后台应用日志 |

业务数据仍在安装目录。**为了开机自启，会在 `/etc/systemd/system/nestbot.service` 安装 root 所有的服务文件，并创建 `multi-user.target.wants/` 启用链接。** 不再承诺系统目录完全零写入；systemd 自身的状态、服务管理记录、SSH 日志及 swap 属于操作系统。

服务不创建专用系统用户，不安装到 `/usr/local`，不把业务数据搬到 `/var`。服务的文件系统写权限限制到安装目录，备份输出也应放在该目录内。不要删除或移动运行中的安装目录，不要分享 `secrets.env` 或整份数据目录。

## 6. Web 管理

在你自己的电脑另开终端，建立并保持 SSH 隧道：

```sh
ssh -L 8787:127.0.0.1:8787 用户名@服务器IP
```

替换用户名/IP；SSH 非 22 端口时加 `-p 端口号`。电脑浏览器打开 http://127.0.0.1:8787 ，使用 Web 管理密码登录。公网服务需要可信 HTTPS 反向代理、`allow_remote=true` 和 `secure_cookie=true`，当前部署脚本不配置反向代理。

## 7. 备份与更新

先生成 SQLite 一致性备份，再停服备份主密钥和配置：

```sh
mkdir -p ~/nestbot/.local/backup
~/nestbot/deploy/run-local.sh backup ~/nestbot/.local/backup/nestbot.sqlite
~/nestbot/deploy/run-local.sh shutdown
cp ~/nestbot/.local/data/master.key ~/nestbot/.local/backup/master.key
cp ~/nestbot/config/nestbot.toml ~/nestbot/.local/backup/nestbot.toml
cp ~/nestbot/config/secrets.env ~/nestbot/.local/backup/secrets.env
# 与首次部署同一命令；不再需要 --no-login
curl -fsSL https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/install.sh | sh
```

自选目录仍需传 `--dir`，否则会部署到默认目录。普通用户部署前再次 `sudo -v`。原密码、会话、数据库及配置保留，更新后服务自动启动。

备份同样敏感，下载到安全位置；同一磁盘内备份不能防 VPS 磁盘丢失。不要只复制运行中的 SQLite 主文件遗漏 WAL；使用 `backup`。回退需匹配版本及更新前完整备份，迁移后的数据库不保证能用旧程序直接打开。

## 资源设计与验收边界

- Tokio 一个主线程、最多两个阻塞线程；SQLite 默认 8MiB 页缓存，WAL/FULL 同步，磁盘临时存储。
- 主账号协议任务串行，Bot copy 独立通道；更新/广播/管理缓冲有界，媒体定位先落 SQLite，最多按 10 条读取处理，下载块 512KiB。
- 默认临时媒体配额 5GiB、单文件上限 4GiB、磁盘保留 256MiB，磁盘与 1GB 内存是两个独立要求；优先 copy。
- 去重按类型索引，转存与过期清理使用数据库索引。成功且无不确定发送的任务事务性回收临时记录，其他状态保留；保留完成账本、历史、报告和搜索选择。
- 两账号受限提取轮换遵守冷却、等待可取消；进程重启后从主账号开始。

真实 Telegram 登录、权限、传输峰值及 0.5 核/1GB 的 72 小时稳定性仍需目标 VPS 实测。开机自启注册及实际服务运行通过 systemd 验证，不将测试虚拟机上的停启检查冒充一次真实 VPS 重启。
