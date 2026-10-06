# 归巢 NestBot · Rust

面向 0.5 核、1GB Linux VPS 的 Telegram 搜索与转存服务。Bot、CLI 和轻量 Web 共用一个常驻 Rust 进程。

当前实现包含：逐页搜索入夹和续搜、密钥领取与分组按钮、copy/deep 转存、双账号、持久化任务队列、文件级恢复、相册转发聚合、标签、分页管理页面、CLI 本地控制及旧密钥夹导入。

真实 Telegram 链路和 VPS 资源指标需要在目标环境验收。重传表示下载后重新上传，不保证平台存储物理独立或免于平台清理。

## 本地启动

```powershell
# 本工作区工具链已经装在 .local/tools；其他机器可使用普通 cargo。
.\scripts\cargo-local.ps1 build --release --locked
Copy-Item config/nestbot.example.toml config/nestbot.toml
Copy-Item config/secrets.example.env config/secrets.env
# 修改 TOML 中 api_id、allowed_users、default_target、search_bot、file_bot；填写 secrets.env。
.\target\release\nestbot.exe --env-file config/secrets.env init
.\target\release\nestbot.exe --env-file config/secrets.env login
# 可选上传账号；上传账号须已加入目标聊天。
.\target\release\nestbot.exe --env-file config/secrets.env login --upload
.\target\release\nestbot.exe --env-file config/secrets.env serve
```

Web 默认地址：http://127.0.0.1:8787。管理密码至少 12 字符；密钥夹密码必须长期保存。凭据文件不进入版本控制，程序日志不输出凭据、关键词或媒体内容。

账号登录在服务停止时通过 CLI 完成。服务运行期间，其他 CLI 命令通过本地控制接口操作同一个进程，不另开 Telegram 连接。

```powershell
.\target\release\nestbot.exe status
.\target\release\nestbot.exe search "合成关键词" --pages 2
.\target\release\nestbot.exe search "合成关键词" --all
.\target\release\nestbot.exe search "合成关键词" --resume
.\target\release\nestbot.exe fetch synthetic-key --mode copy --target=-1001234567890
.\target\release\nestbot.exe batches
.\target\release\nestbot.exe entries BATCH_ID --start 1 --limit 50
.\target\release\nestbot.exe fetch --batch BATCH_ID --start 1 --end 10 --mode deep --target=-1001234567890
.\target\release\nestbot.exe stop JOB_ID
.\target\release\nestbot.exe retry JOB_ID
.\target\release\nestbot.exe set target -1001234567890
.\target\release\nestbot.exe backup .local/backup.sqlite
```

`entries` 会显示本人需要查看的明文，请勿把输出贴进公开日志。`retry --allow-uncertain` 必须先检查目标是否已经收到文件，重试可能产生重复副本。

CLI 搜索默认一页；Bot `/search` 默认搜全页。原 Python 业务规则的对齐范围、数据库兼容与离线测试记录见 [Python 行为对齐](docs/python-parity.md)。

## Bot

配置 `telegram.allowed_users` 和 `TELEGRAM_BOT_TOKEN`；白名单为空时不接受任务。把 bot 加入目标频道并授予发消息及编辑权限。

支持 `/search`、`/grab`、`/fetch`、`/batch`、`/copy`、`/deep`、`/mode`、`/bind`、`/target`、`/status`、`/stop`、`/clear`、`/retry`、`/chats`、`/log`。转发媒体后自动转存，发送 `#标签` 给最近一批补标。Bot API copy 任务拥有独立执行通道，大文件 deep 任务不会阻塞它。

## Linux 部署

在开发机或 CI 构建 Linux release，不在小 VPS 上编译。CI 生成包含程序和部署文件的压缩包。详见 [部署说明](docs/deployment.md)。

```sh
sudo sh deploy/install.sh
# 修改 /etc/nestbot/nestbot.toml 和 /etc/nestbot/secrets.env
sudo -u nestbot nestbot --config /etc/nestbot/nestbot.toml --env-file /etc/nestbot/secrets.env init
sudo -u nestbot nestbot --config /etc/nestbot/nestbot.toml --env-file /etc/nestbot/secrets.env login
sudo systemctl enable --now nestbot
```

systemd 默认限制 CPU 50%、内存 512MiB；Web 使用 SSH 隧道访问。数据库和账号会话在 `/var/lib/nestbot`，临时媒体在 `/var/cache/nestbot`，控制 socket 在 `/run/nestbot`，日志进入 journald。

## 目录与旧版迁移

`src/` 按应用服务、任务调度、Telegram、存储、接口和日志拆分；`web/` 为内嵌静态页面；`config/` 为模板；`deploy/` 为 Linux 部署文件；`migrations/` 为数据库结构；`tests/` 为 Rust 集成测试；本地运行数据全部放在 `.local/`。

原本的本地工作区中，Python 源码、测试、依赖环境和原运行数据完整保留在 `legacy/python/`；这些内容不随公开仓库发布。仅在保留了旧版的本地工作区，可使用 `scripts/run-legacy.ps1` 或原启动批处理运行。详情见 [迁移说明](docs/migration.md)。

```powershell
# 停止 Rust 服务后导入。不会修改旧文件。
.\target\release\nestbot.exe --env-file config/secrets.env import-legacy legacy/python
```

旧 `HV1` 密钥夹在用户本机解密后加密导入；旧进度因缺少目标和模式，单独保留，不能自动用于跳过。原 Telethon 会话不自动迁移，新版通过 CLI 重新登录。

## 验证

```powershell
.\scripts\cargo-local.ps1 fmt --all -- --check
.\scripts\cargo-local.ps1 clippy --offline --all-targets -- -D warnings
.\scripts\cargo-local.ps1 test --offline --all-targets
```

测试只使用独立临时数据库及合成数据，不访问真实会话、密钥夹或 Telegram。实现边界和验收项目见 [验收说明](docs/acceptance.md)。
