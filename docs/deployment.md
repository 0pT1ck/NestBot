# Linux 单目录部署与资源预算

## 1. 准备信息

建议 Debian 12/13 或 Ubuntu 22.04/24.04，64 位 x86_64 或 ARM64，至少 1GB 内存。程序发布为静态 musl 二进制，不在 0.5 核 VPS 上编译，不需要安装 Rust、Python、Docker 或数据库服务。真实 Telegram 转存的内存与长期稳定性仍须在目标 VPS 实测。

先准备以下信息，不要把凭据或验证码发给别人：

| 安装时询问 | 去哪里获取 / 填什么 |
| --- | --- |
| API ID、API Hash | 登录 https://my.telegram.org → API development tools；一个数字 ID 和 32 位 Hash。两个账号共用这一组，不需要给第二账号再申请 App ID。 |
| Bot Token | 在 Telegram 的官方 `@BotFather` 创建你的管理机器人后获取；不是 API Hash。 |
| 你的 Telegram 用户 ID | 数字用户 ID，不是手机号、用户名或频道 ID；用于只允许你操作管理机器人。 |
| 搜索机器人、文件机器人用户名 | 填实际使用的机器人名称，不填 `https://t.me/...`；可带开头的 `@`。 |
| 默认转存目标 | `-100...` 聊天 ID 或公开 `@channel_name`；可留空，启动后在 Bot 中设置。 |
| 数据库解锁密码 | 自己设置，至少 12 字符；用于解锁主密钥，初始化后不能直接改。必须备份保存。 |
| Web 管理密码 | 自己设置，至少 12 字符；与数据库密码是不同用途。 |
| SOCKS5 代理 | VPS 能直连 Telegram 就留空；否则填完整 `socks5://...` URL。 |

第一个账号及可选第二个账号需准备手机号、Telegram 验证码，有二步验证则还需二步验证密码。将管理 Bot 加入目标频道并授予发消息等必要权限；账号也必须有对应目标权限。

## 2. 登录 VPS，执行一行命令

在你的电脑使用 SSH 客户端连接 VPS，打开可输入文字的 SSH 终端。下面命令在 **VPS 终端** 执行，不是在 Telegram 里发送。

```sh
curl -fsSL https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/install.sh | sh
```

默认安装到当前用户的 `$HOME/nestbot`：普通用户通常是 `/home/用户名/nestbot`，root 是 `/root/nestbot`。不用额外加 `sudo`。需要安装在指定位置时，用以下命令替代上面那条，先将路径换成自己拥有写权限的目录：

```sh
curl -fsSL https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/install.sh | sh -s -- --dir /xxx/nestbot
```

`/xxx/nestbot` 是自选路径示例，不是强制目录。目录太深可能超过 Linux Unix socket 路径限制，优先使用默认短路径。不支持 32 位机器。

如果提示 `curl: command not found`，Debian/Ubuntu 先执行：

```sh
sudo apt update && sudo apt install -y curl ca-certificates
```

root 用户没有 `sudo` 时去掉 `sudo`。安装系统软件包本身会写系统目录，这是你调用系统包管理器的行为，不是 NestBot 把数据写到系统目录。脚本还需常见系统命令 `tar`、`gzip`、`sha256sum`、`mktemp`、`nohup`、`ps`、`stty`；标准 Debian/Ubuntu 通常已具备，精简系统缺失时按错误提示安装。

脚本会自动识别架构，下载公开 Release 并校验 SHA-256，然后逐项询问配置。密码输入不显示字符，正常输入后按回车，再确认一次。手机号按登录提示填写；验证码可能发到 Telegram App，不一定是短信。第一个账号登录后询问是否登录第二账号，按回车默认登录，输入 `n` 跳过。第二账号仍复用现有第二/上传会话，受限轮换只影响提取，不改变搜索、上传选择。

最终出现“启动成功”后可以关闭 SSH，程序继续后台运行。重复执行安装命令时，若服务已运行，不重复启动、不下载替换程序；已有配置和密钥不覆盖。下载失败或校验失败时停止，不使用未校验程序。`--no-login` 仅供已经准备好配置/会话的部署跳过登录，不能用于首次交互配置。

## 3. 文件在哪里

所有入口都固定工作目录为安装目录，并设置目录内的 `TMPDIR`、`SQLITE_TMPDIR`，关闭普通 core dump。

| 相对于安装目录的路径 | 内容 |
| --- | --- |
| `nestbot`、`deploy/`、`docs/` | 程序、脚本、说明 |
| `config/nestbot.toml` | 目标、用户白名单、API ID、机器人名称等配置 |
| `config/secrets.env` | API Hash、Bot Token、两种密码、可选代理；按文本加载，不作为 shell 执行 |
| `.local/data/` | SQLite、WAL、加密会话、主密钥 `master.key`、进程锁 |
| `.local/cache/` | deep 转存的临时媒体 |
| `.local/run/` | 控制 socket、服务发现、后台 PID、操作锁 |
| `.local/tmp/` | 下载校验的临时包、程序与 SQLite 临时文件；安装完成清理临时包 |
| `.local/log/nestbot.log` | 后台运行日志 |

不创建系统用户，不注册 systemd，不往 `/etc`、`/usr/local`、`/var`、`/run` 安装程序或业务数据。前提是你不把配置路径、备份或导入输出主动改到别处。系统自己的 SSH/审计日志、swap、崩溃收集服务不在程序控制范围内。

不要删除 `.local/data/master.key`，不要在初始化后直接更改数据库密码，也不要分享 `secrets.env` 或整份数据目录。

## 4. 日常启动、查看与停止

默认目录用下面命令；指定安装目录时把 `~/nestbot` 替换成自己的路径。无需先 `cd`，可从任意目录执行。

```sh
# 启动；已运行不会再启动一份
~/nestbot/deploy/run-local.sh start
# 查看状态
~/nestbot/deploy/run-local.sh status
# 停止整个服务，等它正常退出
~/nestbot/deploy/run-local.sh shutdown
# 重启整个服务
~/nestbot/deploy/run-local.sh restart
# 查看最后 100 行日志
 tail -n 100 ~/nestbot/.local/log/nestbot.log
```

`stop` 和 Telegram `/stop` 是停止任务，**不是停止整个服务**。重新登录账号前先 `shutdown`，再执行 `login` 或 `login --upload`，完成后 `start`。启动/停止使用 PID 与命令行归属校验，不会因 PID 被其他进程复用而贸然杀掉它。

后台模式不会自动开机启动、崩溃重启、轮转日志或施加 cgroup CPU/内存硬限额。VPS 重启后重新 SSH 登录执行 `start`；不要把“退出 SSH 后继续运行”理解成“重启系统后自动运行”。可另行配置系统服务，但注册服务文件和 journald 日志会属于系统目录，不再是纯单目录安装。

## 5. 打开 Web 管理页面

默认只监听 VPS 本机 `127.0.0.1:8787`，避免直接暴露公网。**在你自己的电脑** 打开另一个终端，保持下列连接运行：

```sh
ssh -L 8787:127.0.0.1:8787 用户名@服务器IP
```

替换用户名和服务器 IP，例如平常登录 VPS 使用的用户名；随后在电脑浏览器打开 http://127.0.0.1:8787 ，输入安装时设置的 Web 管理密码。SSH 非 22 端口时加 `-p 端口号`。不要为了方便直接改成 `0.0.0.0`；公网部署需可信 HTTPS 反向代理、`allow_remote=true`、`secure_cookie=true`，当前一键安装不配置这些。

## 6. 备份与更新

更新前先生成 SQLite 一致性备份，并安全保存 `master.key`、配置和数据库密码：

```sh
mkdir -p ~/nestbot/.local/backup
~/nestbot/deploy/run-local.sh backup ~/nestbot/.local/backup/nestbot.sqlite
~/nestbot/deploy/run-local.sh shutdown
cp ~/nestbot/.local/data/master.key ~/nestbot/.local/backup/master.key
cp ~/nestbot/config/nestbot.toml ~/nestbot/.local/backup/nestbot.toml
cp ~/nestbot/config/secrets.env ~/nestbot/.local/backup/secrets.env
# 已有会话无需重复登录；更新后自动启动
curl -fsSL https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/install.sh | sh -s -- --no-login
```

指定安装目录的更新命令仍需加 `--dir /xxx/nestbot`，否则会使用默认目录。备份文件同样敏感，妥善保存并下载到安全位置；仅放在同一块磁盘不能防止 VPS 磁盘丢失。不要只复制运行中的 SQLite 主文件而遗漏 WAL；使用 `backup` 命令。

发布包不包含真实配置或 `.local/`，更新不覆盖它们。更新会替换程序、脚本、模板和说明，因此不要在这些发布文件里存放自己的数据。数据库迁移不保证能用旧二进制直接回退，回退需匹配版本及更新前完整备份。

## 资源预算

- Tokio 主运行时一个线程，阻塞线程最多两个；SQLite 页缓存默认 8MiB，WAL、FULL 同步、磁盘临时存储。
- 主账号协议任务串行，Bot API copy 使用单独通道；MTProto 更新缓冲 64、应用消息广播 128、管理事件 64。
- Bot 每次最多领取 20 个更新，排队任务最多 1024；Web 同时最多 32 个请求、8 个事件流、16 个登录会话，密码校验串行。
- 收取的媒体定位信息先存 SQLite，再按最多 10 条读取；下载块 512KiB，避免整文件驻留内存。
- 默认临时媒体总配额 5GiB、单文件上限 4GiB、磁盘保留 256MiB；磁盘与 1GB 内存是两个独立要求。优先使用 `copy`，避免不必要的下载和重新上传。
- 两账号提取严格主 → 第二 → 主，成功后保持当前账号；切回尚在冷却的账号则等待，等待可取消。仅提取轮换，重启后从主账号开始。
- 去重按媒体类型建立有序 ID 集合，保留旧 `file_ids` 数组 JSON；转存及过期清理查询使用索引。
- 已完成且没有不确定发送的任务事务性清理 `claim_inbox` 和 `job_media_seen`；其他状态保留。启动清理历史成功任务的临时数据、过期按钮和超过 7 天的 Bot 更新，不新增周期性全库扫描或自动 VACUUM。完成账本、历史、报告和搜索选择保留。

以上是设计约束，不是 0.5 核 1GB VPS 的实测保证。空闲 RSS、deep 转存峰值、文件页缓存及 72 小时稳定性需在目标 Linux 环境测量。
