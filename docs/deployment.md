# 部署与资源预算

## 安装

优先使用 CI 产物，或在 Linux 开发机运行 `cargo build --locked --release`。当前 CI 构建 glibc Linux 二进制，建议在与构建环境兼容的较新发行版运行；不宣称支持任意旧版 glibc。ARM VPS 应在 ARM 构建机另外构建。

`deploy/install.sh` 创建专用用户和目录，不覆盖已有配置，不自动启动。填写 API ID、白名单、目标、账号与 Bot 凭据后，通过 `init`、`login` 完成初始化。

配置文件中的路径与工作目录无关时应使用绝对路径。备份同时包含 SQLite、`master.key` 和解锁密码；失去任一必要密钥会导致数据不可读。数据库备份通过 SQLite backup API 完成，不能只复制运行中的主数据库文件而遗漏 WAL。

## 管理

```sh
ssh -L 8787:127.0.0.1:8787 user@vps
sudo -u nestbot nestbot --config /etc/nestbot/nestbot.toml status
sudo journalctl -u nestbot -n 100
sudo systemctl stop nestbot
sudo -u nestbot nestbot --config /etc/nestbot/nestbot.toml --env-file /etc/nestbot/secrets.env login --upload
sudo systemctl start nestbot
```

公网 Web 需要明确设置 `allow_remote=true` 和 `secure_cookie=true`，并在可信反向代理后提供 HTTPS。默认 SSH 隧道无需额外常驻代理服务。

## 资源上限

- Tokio 主运行时一个线程，阻塞线程最多两个。
- SQLite 页缓存默认 8MiB，WAL、FULL 同步、磁盘临时存储。
- 主账号协议任务串行；Bot API copy 单独一个执行通道。
- MTProto 更新处理缓冲 64，应用消息广播缓冲 128，管理事件缓冲 64。
- Bot 每次最多领取 20 个更新；排队任务最多 1024。
- Web 同时最多处理 32 个管理请求、8 个事件流和 16 个登录会话；密码校验串行，登录每分钟最多 5 次。
- 领取消息先把媒体定位信息保存到 SQLite，再按最多 10 条读取处理，慢速下载不依赖内存消息缓冲保存整批文件。
- 搜索与领取同时使用实时更新和约每 3 秒的聊天历史回读；真正的限流最多自动重试 5 次，普通处理中提示和空搜索结果不触发重发。
- 下载块 512KiB；上传使用协议库的固定数量分块工作器，避免整文件驻内存。
- 默认临时文件总配额 5GiB，最大单文件 4GiB，磁盘保留 256MiB。磁盘空间与 1GB 内存是两个独立要求。
- systemd `CPUQuota=50%`、`MemoryHigh=384M`、`MemoryMax=512M`。

内存目标应同时记录进程 RSS 与 systemd/cgroup 内存，文件页缓存属于后者。容量限制属于设计约束；是否满足 128MiB 空闲 RSS、256MiB 转存 RSS，需要真实 Linux 测量。

## 更新与回退

先停止服务和备份数据库与主密钥，再替换二进制。原 Python 数据没有被转换覆盖，回退运行 `legacy/python` 即可；Rust 新产生的任务与数据不会自动回写 Python。
