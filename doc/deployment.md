# NestBot Linux 单目录部署

## 支持范围与限制

发布目标是普通用户拥有的单个部署目录，不需要 sudo、Rust 工具链、systemd、Docker 或独立数据库服务。计划发布 **Linux x86_64 / aarch64、glibc >= 2.39** 的二进制，使用 Ubuntu 24.04 runner 构建，适用于 Ubuntu 24.04、Debian 13 或满足相同架构及动态运行库要求的系统；不是全 Linux 通用包，不支持 Alpine/musl 或更旧的 glibc。

本地 `install.sh` 不负责下载或解压归档，不注册开机启动或创建系统用户；在线引导入口 `bootstrap.sh` 负责下载、校验、解压并调用安装器，所有暂存文件都留在所选根目录内。脚本依赖 Linux 上的 POSIX `sh`、GNU coreutils（含 `readlink`、`stat`、`sha256sum`、`nohup`）及 GNU `find`；在线引导还需 `curl`、`tar`，打包另需 `file`、`tar`。平台/版本检查不能替代目标机器的运行库及实际运行验收。

首版只支持**新安装与同一完整发布包的幂等重跑**。不同版本、受管理文件有变动、未知非空目录以及正在运行的实例，安装器都会拒绝处理。**没有自动升级、回滚、备份、保留策略、进程守护或开机自启**。不要把新包覆盖解压到现有实例来“升级”；即使先停机，首版也不支持替换程序与状态迁移。`backups/` 仅为目录约定，不意味着实现了备份功能。

脚本实现不等于端到端验收。当前 Windows/Git Bash 环境只能做 shell 语法与拒绝非 Linux 的检查；未在真实 Linux VPS 完成安装、登录、启动停止、迁移、libc 兼容性、日志上限与 RSS/PSS 验收。Telegram 认证、双次原生转发及低内存目标还需要独立真机验证。

## 目录布局

```text
<root>/
  bin/nestbot
  config/config.example.toml   # 发布样例，包含在 SHA256SUMS 中
  config/config.toml           # init 仅在不存在时创建；自行填写凭据
  data/                       # session、关注规则及数据库侧文件
  logs/                       # Rust 写入的有容量限制的日志
  tmp/                        # 本实例临时文件
  run/                        # 锁、stop.request、installed.sha256 发布标记
  backups/                    # 预留，首版没有自动备份
  scripts/install.sh
  scripts/start.sh
  scripts/stop.sh
  scripts/status.sh
  doc/deployment.md
  SHA256SUMS
```

目录创建权限为 0700；安装器复制的发布配置/清单/文档为 0600，程序和脚本为 0700。运行与敏感文件权限由 Rust 负责。全部应用本地路径固定在同一个根目录下；不支持把 session、日志或数据库单独指向其他目录。安装器保守拒绝根路径组件和部署树中的所有符号链接，包括指向根内的链接。不要与其他不可信用户共享写权限；不支持与改动部署树的其他进程并发安装。`--root` 可以含空格，必须加引号，不接受路径中的 `..`。

Telegram 云端中转消息不是本地数据，本工具不下载媒体；单目录约束也不控制操作系统审计或用户自行运行的第三方工具。

## GitHub 发布与一行安装

仓库：[https://github.com/0pT1ck/NestBot](https://github.com/0pT1ck/NestBot)。计划由 GitHub Actions 在推送 `v0.1.0` tag 后构建发布：x86_64 与 aarch64 分别使用 Ubuntu 24.04 对应架构的 runner。预期 Release 资产为 `nestbot-v0.1.0-x86_64-linux.tar.gz`、`nestbot-v0.1.0-aarch64-linux.tar.gz` 及归档摘要总清单 `SHA256SUMS`。**只有工作流成功且资产上传后才有可用发布包；文档中的命令不代表发布已经完成。**

计划的一行安装入口：

```sh
mkdir -p "$HOME/NestBot" && curl -fsSL https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/bootstrap.sh -o "$HOME/NestBot/.bootstrap.sh" && sh "$HOME/NestBot/.bootstrap.sh" --root "$HOME/NestBot"
```

此命令会执行公开仓库中的引导脚本，应先核验来源。脚本默认固定选择 `v0.1.0`，不查询或跟随 `latest`；命令中的 `main` 是引导脚本来源，不代表自动安装最新发布版本。引导脚本在 `$HOME/NestBot/tmp/bootstrap.XXXXXXXX/` 内下载、校验并解压；下载的 `.bootstrap.sh` 和暂存文件均限制在应用根内。引导通过安装器专用的 `--bootstrap-stage` 入口完成新安装，而不是向已有实例覆盖解压。未知非空目录及不兼容平台仍须拒绝。

安装完成后须编辑 `$HOME/NestBot/config/config.toml`，再按下文执行 `check`、交互 `login` 和启动脚本。此入口不替代 Telegram 认证，不支持自动升级。公开仓库只发布本次新项目的当前历史，不包含旧项目历史或实际凭据、session。

## 生成发布包（开发机）

必须先在 Linux 上构建 release，不能把 Windows `.exe` 改名后当作 Linux 程序。所有构建输出保持在仓库 `target/`，不使用指向仓库外的 `CARGO_TARGET_DIR`。

```sh
# 在仓库根目录执行；依赖构建本身可能需要联网。
sh scripts/cargo-local.sh build --release
sh scripts/package.sh
# 使用另一个已构建且可在当前 Linux 主机执行的 release 产物：
# sh scripts/package.sh --binary "$PWD/target/x86_64-unknown-linux-gnu/release/nestbot"
```

打包不调用 Cargo、不联网。检查 `file` 报告为 ELF，并实际执行 `--version`，要求其报告的平台与 Linux 主机架构相符；首版打包接受 x86_64、aarch64。跨架构但无法在打包机执行的文件会被拒绝。此检查不代替目标机运行库测试。

输出为唯一命名的 `target/package/nestbot-<架构>-linux.<随机串>/` 和仓库 `target/` 下同名 `.tar.gz`；不会覆盖既有包。包只含程序、样例、四个部署脚本、本文及 `SHA256SUMS`，不包含真实配置、session 或构建缓存。包内七个固定文件都列在清单中。打包同时打印归档 SHA-256；发布者应通过可信渠道单独提供该摘要。失败可能留下不完整的目录/归档，应检查命令退出状态；不会自动清理旧包，需由维护者管理磁盘占用。

## 首次安装

1. 通过可信渠道取得发布包以及独立的归档 SHA-256。先验证完整归档，包括其中的安装脚本。包内 `SHA256SUMS` 用于检测损坏，并非签名；攻击者若能修改脚本和清单，内部校验不能提供真实性保证。
2. 用户自行检查归档条目，仅向**新建的空目录**解压，拒绝绝对路径、`..`、符号链接、硬链接和特殊文件。下面的解压命令仅用于已验证、可信的本项目发布包，**不是通用的不可信 tar 安全解压器**。
3. 以拥有目录的普通用户执行安装，不要用 sudo。

例如，发布包已在用户指定部署根内的独立下载暂存目录（如已有实例的 `tmp/release-唯一值/`），或其他由用户自行管理的位置准备好。安装脚本自身不负责下载、传输或解压，不会将临时文件写入系统 `/tmp`。

```sh
# ARCHIVE 为已验证的包路径；ROOT 为尚不存在的最终部署目录。
# 父目录需事先存在，并由用户自行选择和管理。
ROOT='/home/bot/my NestBot'
ARCHIVE='/path/to/verified/nestbot-x86_64-linux.UNIQUE.tar.gz'
# 按可信渠道的值校验：
sha256sum "$ARCHIVE"
# 检查归档名称/类型；确认它确实来自可信发布者后再解压。
tar -tzvf "$ARCHIVE"
(umask 077; mkdir "$ROOT")
tar --no-same-owner --no-same-permissions -xzf "$ARCHIVE" -C "$ROOT"
sh "$ROOT/scripts/install.sh" --root "$ROOT"
```

发布包直接解压后的目录虽然非空，但仅包含固定清单条目，因此安装器允许这种正常首装；其他未知非空目录被拒绝。也可从可信的已解压包调用安装器，指定另一个空目录或不存在的目标目录（其父目录必须存在），安装器会复制七个发布文件和清单，再初始化。目标不得是源发布包的子目录（包括源包的 `tmp/`），避免嵌套实例及递归安全检查互相包含；可以原地安装，或者选择与源包不嵌套的独立目标目录。源包放在已有实例的 `tmp/` 内不代表支持升级：安装器仍只允许清单完全相同、停止状态的幂等重跑。

安装先检查平台、目录所有者/写权限、符号链接、严格清单和 SHA-256；随后在选定根内创建目录，记录 `run/installed.sha256` 并调用：

```sh
"$ROOT/bin/nestbot" --root "$ROOT" init
```

`init` 仅用 create_new 语义创建不存在的 `config/config.toml`，不覆盖配置、session 或规则。安装不运行 `check`、不登录、不自动启动，因为初始样例仍需用户填写。若初始化在发布标记写入后中断，相同包可以重试；复制/落盘阶段中断留下的不完整目录可能被明确拒绝，首版不承诺事务式安装恢复，也不会清空或强制覆盖它。

## 配置、登录与启动

先在 Telegram 创建 Bot、取得 API ID/hash，准备个人账号、所有者 ID 和私有中转超级群。按配置样例说明编辑 `config/config.toml`，不要将 token、api_hash、验证码、2FA 或 session 放进公开命令记录、日志、Git 或聊天。

```sh
# ROOT 继续使用上节同一个值。
"$ROOT/bin/nestbot" --root "$ROOT" check
"$ROOT/bin/nestbot" --root "$ROOT" login
sh "$ROOT/scripts/start.sh" --root "$ROOT"
sh "$ROOT/scripts/status.sh" --root "$ROOT"
sh "$ROOT/scripts/stop.sh" --root "$ROOT"
```

统一命令顺序是 `nestbot --root PATH 子命令`，不要交换顺序。`check` 只校验本地配置，不发起登录；`login` 前台交互获取验证码/2FA。`run` 不应在后台等待首次认证。

四个部署脚本默认以自身实际位置的父目录作为根，从任意当前工作目录调用都可；也可以明确传 `--root`。直接执行发布二进制时 Rust 可从真实 `<root>/bin/nestbot` 推导根，但上面的示例始终显式指定以避免歧义。开发环境从 `target/` 启动必须传仓库根。

`start.sh` 先调用 `check` 和 `status`，只有 status 返回停止才启动：

```sh
nohup "$ROOT/bin/nestbot" --root "$ROOT" run </dev/null >/dev/null 2>&1 &
```

标准输出/错误必须丢弃到 `/dev/null`，不会生成无限增长的 `nohup.out`；**错误诊断和有界日志由 Rust 在 `logs/` 写入**，不是 shell 重定向实现。最多轮询约 10 秒（检测成功后额外确认 1 秒），用 Rust 锁状态确认运行。此处成功仅表示**进程已启动并持有单实例锁**，不表示 Telegram 连接 ready 或转发验收通过。连接过程可能超过 11 秒，并在脚本已经报告成功后失败退出；额外 1 秒复查只排除部分立即退出情况。应结合后续 status、`logs/` 及真实测试消息验证运行情况，不保证进程之后不会退出。超时不 kill 任何进程；请先重新检查 status，再决定 stop 或重试。脚本没有守护、自动重启或系统级启动项。

status 退出码为 `0`（运行）、`3`（已停止），其余非零是错误，不能把所有非零都当作“已停止”。停止脚本直接委托 Rust 校验单实例锁、写入根内 `run/stop.request` 并等待锁释放；不读陈旧 PID 来发信号，不使用 `kill`、`pkill` 或 `killall`。停止超时时应检查日志和状态，不手工删除锁绕过单实例约束。运行与停止过程没有 shell 侧 PID 控制文件。

## 幂等重跑、迁移与维护

- 先执行 stop，确认 status 返回 3，才能重跑安装；运行中即使版本相同也拒绝安装。
- 重跑要求 `run/installed.sha256` 与发布清单一致，全部受管理文件 SHA-256 正确且与源包一致。保留已有 `config/config.toml`、`data/`、日志与规则；不覆盖用户凭据。
- 缺失发布标记、修改发布脚本/样例或更换版本不能由安装器“修复”；会明确拒绝，而不是假装完成升级。不要手工伪造标记来强制安装。
- 没有备份自动化；维护者要先停止并确认数据库关闭，再自行制作一致性副本。备份应包含版本/程序、配置和完整状态，排除 `backups/`、`tmp/`、`run/` 和旧日志，防止递归打包和误用运行中的单个 SQLite 主文件。
- 整目录迁移需先停止旧实例、确认锁释放，再由用户复制整个目录并保留敏感权限和所有权。新位置的 `--root` 改为新目录，先 check/login 状态再启动；不要同时启动旧副本。迁移不代表已验证跨版本状态兼容，首版不承诺自动迁移或回滚。
- 排障优先看 `logs/`；检查配置、session、目录权限、磁盘容量及二进制运行库。若二进制在加载器阶段就无法启动，Rust 尚未能写日志，可在终端直接运行 `"$ROOT/bin/nestbot" --version` 获取平台/加载错误。

## 待在 Linux 完成的验收

普通非 root 用户、新解压非空目录、另选空目录、未知非空拒绝、同包重复安装、不同包拒绝、运行中拒绝、带空格路径及任意 cwd；错误清单/缺失文件/重复条目/校验失败；目录 symlink 及权限拒绝；Windows 或错误架构二进制拒绝；首次配置与登录、启动失败可诊断、双实例锁、无 nohup.out、正常停止及超时；根外不可写条件下的路径审计、日志轮转上限、session 与规则停机迁移；实际 Telegram 功能与 Linux release RSS/PSS。语法检查通过不能替代这些验收。
