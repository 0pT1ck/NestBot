# 归巢 NestBot · Rust

面向 0.5 核、1GB Linux VPS 的 Telegram 搜索与转存服务。Bot、CLI 和轻量 Web 共用一个常驻 Rust 进程。

当前实现包含：逐页搜索入夹和续搜、密钥领取与分组按钮、copy/deep 转存、单账号、持久化任务队列、文件级恢复、相册转发聚合、标签、分页管理页面、CLI 本地控制及旧密钥夹导入。

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

同一关键词重复搜索会合并到同一个密钥夹，按密钥去重。不同关键词的相同密钥共享完成记录，因此新密钥夹也可能已有部分“已转”。搜索翻页停住时任务显示 `partial` 并保留结果；使用 `/search 关键词 continue` 续搜，使用 `/batch` 转存。

搜索 Bot 报“发生错误，请稍后重试”时，保留原消息按钮，等 60 秒后重试，最多 20 轮。等待期间会通知当前轮数，`/status` 可查看剩余等待；同一条旧提示不会重复触发重试。迟到的新页先保存，避免多点一次跳页；重试无新回复或次数耗尽时会停止并提示，`/stop` 可随时取消。

搜索和转存的排队消息会持续编辑更新（约每 2 秒，内容未变化时不发送）：搜索显示当前页和总页数，批量转存显示密钥夹内正在处理的密钥序号。搜索、下载、上传或 Bot API 要求等待 `x` 秒时，当前任务显示剩余等待，并在 `x + 60` 秒后自动继续原请求；等待不计入正常超时，`/stop` 可随时取消。普通网络错误不会自动重发不确定的发送。

## 单账号与风控等待

仅使用 `login` 登录的主账号进行搜索、提取、上传和转发；第二账号及 `login --upload` 已删除。主账号原有会话继续使用，无需因升级重新登录；数据库里遗留的旧辅助会话不会被读取或启用，也不会删除用户历史数据。

Telegram RPC、Bot API 或文件机器人返回等待时间 `x` 时，严格等待 `x + 60` 秒再继续，不切账号、不将长等待截短，不把返回的 0 秒改为 1 秒。RPC 重试保留原请求及随机 ID；文件机器人文字/按钮限流后重试当前密钥，已完成媒体保留并去重。限流等待不消耗正常请求超时，`/stop` 可取消。没有明确数值的限制提示仍使用既有默认等待值。

## Bot

配置 `telegram.allowed_users` 和 `TELEGRAM_BOT_TOKEN`；白名单为空时不接受任务。把 bot 加入目标频道并授予发消息及编辑权限。

支持 `/search`、`/grab`、`/fetch`、`/batch`、`/copy`、`/deep`、`/mode`、`/bind`、`/target`、`/status`、`/stop`、`/clear`、`/retry`、`/chats`、`/log`。转发媒体后自动转存，发送 `#标签` 给最近一批补标。Bot API copy 任务拥有独立执行通道，大文件 deep 任务不会阻塞它。

批量转存支持批次 ID、关键词和已导入的旧 `.bin` 文件名，序号从 1 开始，起止都包含。例如 `/batch test 25 25` 只处理第 25 条。未设置目标时，先 `/bind 群ID` 或配置 `default_target`。

`/batch test progress 11` 将该批次前 11 条设为已转，其余设为待转；随后 `/batch test`、`/batch continue` 或批次按钮从第 12 条开始。`progress 0` 重置为全部待转；支持调高和调低，数据在重启后保留。修改进度不发送文件、不伪造文件转存记录，也不修改其他批次；正在转存的同一批次需先停止。显式序号段仍按指定范围处理，已完成项用 `redo` 可强制重转。

## Linux 部署

在 systemd Linux 的 SSH 终端执行，无需克隆或编译；普通用户先执行 `sudo -v` 授权服务注册，root 无需：

```sh
curl -fsSL https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/install.sh | sh
```

默认部署到 `$HOME/nestbot`，自动选择 Linux x86_64/aarch64 静态程序并校验 SHA-256。脚本不询问凭据、不登录账号、不用示例密码初始化数据库；只部署并注册 systemd 后台服务及开机自启。指定目录可用 `sh -s -- --dir /xxx/nestbot`。已有配置、密码、会话和数据保留，下载校验成功后才停止原服务更新。

首次部署后，直接编辑 `config/nestbot.toml`（API ID、白名单、目标、机器人名称）和 `config/secrets.env`（API Hash、Bot Token、两种密码等）。再执行 `~/nestbot/deploy/run-local.sh init`、`login`，最后 `start`。手机号、验证码、二步验证密码仅由独立登录命令询问，支持 Ctrl-H/DEL 退格；只登录主账号。

业务文件仍在安装目录；开机自启会额外安装 `/etc/systemd/system/nestbot.service` 及启用链接，服务状态和管理记录属于系统。应用日志留在 `.local/log/nestbot.log`。systemd 异常退出重启，CPU 限制 50%、内存硬上限 512MiB；不自动轮转日志。新安装缺少主密钥时暂不启动，完成配置后启动即可在以后开机自动运行。

启动、状态、停止、重启使用 `deploy/run-local.sh start`、`status`、`shutdown`、`restart`；`stop` 仍只停止任务。登录前先停服，不要另起 nohup/serve 实例。完整编辑步骤、登录、自启、备份和 SSH 隧道见 [部署说明](docs/deployment.md)。

启动时自动补齐转存状态及过期记录查询索引。任务确认完成且没有不确定发送后，回收该任务的临时收取记录和任务内去重记录；启动恢复时也回收历史已完成且没有不确定发送任务的这些数据，并清理过期按钮与超过 7 天的 Bot 更新记录。失败、取消及需核对任务的恢复数据保留；完成账本、任务历史、报告和搜索选择范围不删除。旧密钥完成记录的 JSON 格式保持兼容。

## 目录与旧版迁移

`src/` 按应用服务、任务调度、Telegram、存储、接口和日志拆分；`web/` 为内嵌静态页面；`config/` 为模板；`deploy/` 为 Linux 部署文件；`migrations/` 为数据库结构；`tests/` 为 Rust 集成测试；本地运行数据全部放在 `.local/`。

原本的本地工作区中，Python 源码、测试、依赖环境和原运行数据完整保留在 `legacy/python/`；这些内容不随公开仓库发布。仅在保留了旧版的本地工作区，可使用 `scripts/run-legacy.ps1` 或原启动批处理运行。详情见 [迁移说明](docs/migration.md)。

```powershell
# 停止 Rust 服务后导入。不会修改旧文件。
.\target\release\nestbot.exe --env-file config/secrets.env import-legacy legacy/python
```

旧 `HV1` 密钥夹在用户本机解密后加密导入；已导入的旧完成记录按 Python 的领取参数参与跳过和续传。缺少旧记录时可用 `/batch 名称 progress N` 手动校准批次进度。原 Telethon 会话不自动迁移，新版通过 CLI 重新登录。

## 验证

```powershell
.\scripts\cargo-local.ps1 fmt --all -- --check
.\scripts\cargo-local.ps1 clippy --offline --all-targets -- -D warnings
.\scripts\cargo-local.ps1 test --offline --all-targets
```

测试只使用独立临时数据库及合成数据，不访问真实会话、密钥夹或 Telegram。实现边界和验收项目见 [验收说明](docs/acceptance.md)。
