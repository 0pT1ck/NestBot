# 部署与资源预算

## 安装

优先使用 CI 产物，或在 Linux 开发机运行 `cargo build --locked --release`。当前 CI 构建 glibc Linux 二进制，建议在与构建环境兼容的较新发行版运行；不宣称支持任意旧版 glibc。ARM VPS 应在 ARM 构建机另外构建。

`deploy/install.sh` 创建专用用户和目录，不覆盖已有配置，不自动启动。填写 API ID、白名单、目标、账号与 Bot 凭据后，通过 `init`、`login` 完成初始化。

配置文件中的路径与工作目录无关时应使用绝对路径。备份同时包含 SQLite、`master.key` 和解锁密码；失去任一必要密钥会导致数据不可读。数据库备份通过 SQLite backup API 完成，不能只复制运行中的主数据库文件而遗漏 WAL。

### 单目录安装：`/xxx/nestbot`

不运行 `deploy/install.sh`。从仓库 Actions 中下载最新 `main` 提交成功构建的 `nestbot-linux-x86_64` artifact；下载的 ZIP 内包含 `nestbot-linux-x86_64.tar.gz`，先解出该 tar 包再上传 VPS。此产物面向较新 glibc 的 Linux x86_64，不可使用 macOS 二进制；ARM VPS 需另外构建。不要在 0.5 核、1GB VPS 上编译。

以拥有安装目录的普通用户运行，首次安装执行以下命令；已有配置或数据时不要重新复制覆盖：

```sh
mkdir -p /xxx/nestbot
cd /xxx/nestbot
# 将 nestbot-linux-x86_64.tar.gz 放到此目录后解压。
tar -xzf nestbot-linux-x86_64.tar.gz
chmod +x nestbot deploy/run-local.sh
umask 077
cp config/nestbot.example.toml config/nestbot.toml
cp config/secrets.example.env config/secrets.env
chmod 600 config/nestbot.toml config/secrets.env
# 编辑这两个文件：API ID/Hash、Bot Token、allowed_users、目标、机器人用户名及两种密码。
```

保留示例 `[paths]` 中的 `.local/data`、`.local/cache` 和 `.local/run`，不要改成 `/var/...`。`deploy/run-local.sh` 自动把工作目录固定到安装目录，设置私有 `TMPDIR` 和 `SQLITE_TMPDIR`，并关闭 core dump；从其他目录调用也不会改变相对数据路径。全部命令都用该入口：

```sh
./deploy/run-local.sh doctor
./deploy/run-local.sh init
./deploy/run-local.sh login
./deploy/run-local.sh login --upload
# 前台启动，Ctrl+C 停止。
./deploy/run-local.sh serve
```

登录时服务必须停止。需要退出 SSH 后继续运行，可以改用下面的启动方式；不得重复启动：

```sh
cd /xxx/nestbot
mkdir -p .local/log
umask 077
nohup ./deploy/run-local.sh serve >> .local/log/nestbot.log 2>&1 < /dev/null &
./deploy/run-local.sh status
# 停止服务：向上述启动的进程发送 SIGTERM；/stop 只停止任务，不停止服务。
```

配置在 `config/`；SQLite、WAL、加密账号会话、`master.key` 和进程锁在 `.local/data/`；媒体临时文件在 `.local/cache/`；控制 socket/发现文件在 `.local/run/`；SQLite/系统临时文件在 `.local/tmp/`；重定向日志在 `.local/log/`。Web 静态文件内嵌在程序中。程序不安装文件到 `/etc`、`/usr/local`、`/var` 或 `/run`；自定义备份/导入路径仍由用户选择，操作系统自身的审计、SSH 日志及 swap 不在此承诺范围内。

此模式不配置 systemd，也就不会自动获得下面列出的 cgroup CPU/内存硬限制、开机启动和自动重启。需要这些能力时可以另配 systemd 单元，固定 `WorkingDirectory=/xxx/nestbot` 并执行该入口；业务数据仍留在安装目录，但服务注册文件和默认 journald 日志属于系统路径。日志重定向文件不会自动轮转，需要定期归档或清理。

更新前先停止服务并备份数据库、`master.key` 与解锁密码，再解压新的 tar 包；发布包不包含真实 `nestbot.toml`、`secrets.env` 或 `.local/`，不会覆盖它们。不要在服务运行时删除或移动 `.local/data`。

## 管理

```sh
ssh -L 8787:127.0.0.1:8787 user@vps
sudo -u nestbot nestbot --config /etc/nestbot/nestbot.toml status
sudo journalctl -u nestbot -n 100
sudo systemctl stop nestbot
sudo -u nestbot nestbot --config /etc/nestbot/nestbot.toml --env-file /etc/nestbot/secrets.env login --upload
sudo systemctl start nestbot
```

第二账号通过 `login --upload` 登录，复用现有上传会话；请使用不同的 Telegram 账号。主账号提取受限后切换到第二账号，第二账号受限后再切回主账号。只轮换密钥提取，不改变搜索或上传选择；第二账号仍须有目标聊天的发送权限。未登录第二账号时仍使用单账号等待。

公网 Web 需要明确设置 `allow_remote=true` 和 `secure_cookie=true`，并在可信反向代理后提供 HTTPS。默认 SSH 隧道无需额外常驻代理服务。

## 资源上限

- Tokio 主运行时一个线程，阻塞线程最多两个。
- SQLite 页缓存默认 8MiB，WAL、FULL 同步、磁盘临时存储。
- 主账号协议任务串行；Bot API copy 单独一个执行通道。
- MTProto 更新处理缓冲 64，应用消息广播缓冲 128，管理事件缓冲 64。
- Bot 每次最多领取 20 个更新；排队任务最多 1024。
- Web 同时最多处理 32 个管理请求、8 个事件流和 16 个登录会话；密码校验串行，登录每分钟最多 5 次。
- 领取消息先把媒体定位信息保存到 SQLite，再按最多 10 条读取处理，慢速下载不依赖内存消息缓冲保存整批文件。
- 搜索与领取同时使用实时更新和约每 3 秒的聊天历史回读；Bot 排队消息约每 2 秒编辑当前页/密钥序号，未变化不发送。搜索、下载、上传 RPC 和 Bot API 明确要求等待 `x` 秒时，等待 `x + 60` 秒再继续原请求；等待不占用正常超时，显示倒计时且可取消。普通网络错误不会自动重发不确定的发送。
- 文件机器人文字/按钮提示或提取 RPC 限流时，先处理已收取的媒体并保存完成记录，再用另一账号重试当前密钥；成功后保留当前账号，下一次受限才再次切换。切回账号尚在冷却时等待剩余时间；未配置第二账号则等待主账号。冷却和当前提取账号仅保存在常驻进程内，重启后从主账号开始。
- 下载块 512KiB；上传使用协议库的固定数量分块工作器，避免整文件驻内存。
- 默认临时文件总配额 5GiB，最大单文件 4GiB，磁盘保留 256MiB。磁盘空间与 1GB 内存是两个独立要求。
- systemd `CPUQuota=50%`、`MemoryHigh=384M`、`MemoryMax=512M`。
- 文件去重按媒体类型建立有序 ID 集合，查询不反复格式化字符串，也不复制一份完整列表作缓存；旧 `file_ids` 数组格式仍可读取和写回。
- 启动自动升级资源索引：转存账本按 `(job_id,status)` 查询不再全表扫描，Bot 更新及按钮过期清理使用时间索引。
- 已完成且没有不确定发送的任务，在同一事务内清理 `claim_inbox` 和 `job_media_seen`；其他状态保留。启动恢复时回收历史已完成任务的临时数据，清理过期按钮和超过 7 天的 Bot 更新记录。没有新增周期性全库扫描，也不自动 VACUUM；SQLite 可复用已释放页面，但数据库文件不保证立即缩小。完成账本、任务历史、报告和搜索选择范围保留。

内存目标应同时记录进程 RSS 与 systemd/cgroup 内存，文件页缓存属于后者。容量限制属于设计约束；是否满足 128MiB 空闲 RSS、256MiB 转存 RSS，需要真实 Linux 测量。

## 更新与回退

先停止服务和备份数据库与主密钥，再替换二进制。原 Python 数据没有被转换覆盖，回退运行 `legacy/python` 即可；Rust 新产生的任务与数据不会自动回写 Python。
