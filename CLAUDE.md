# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目目标与当前状态

NestBot 使用 Rust 开发面向 Linux 小内存 VPS 的 Telegram 群消息关注机器人，优先控制常驻内存。项目品牌统一为 `NestBot`，Cargo 包名、Rust crate 名和可执行文件名统一为 `nestbot`。

实施计划见 `doc/implementation-plan.md`。现已建立首版 Cargo 工程，实现监听适配、Bot 管理与转发、根内持久化以及 Linux 部署脚本；本地单元/Mock 测试与 CLI 冒烟验证已执行，真实 Telegram、Linux 部署和内存验收尚未完成。实际能力及剩余限制见 `README.md`、`doc/telegram-library-audit.md`、`doc/deployment.md`。历史项目部分文件仍在工作区中处于未提交删除状态，不得自行恢复、清理或将这些删除混入提交。

## 文件位置与归类（强制）

- 开发时 `./` 指本仓库根目录；发布后应用根目录指用户选定的单个部署目录。两者都不等于执行命令时任意的当前工作目录。发布安装过程只写入明确选定的部署目录，不将部署文件散放到其他位置。
- 开发期间所有主动创建或生成的项目文件必须位于仓库内，并按用途归类；部署后所有应用本地文件必须位于部署根内。不得把项目计划、报告、脚本或临时文件写入用户主目录、全局工具目录、系统临时目录或其他仓库。工具自身不可配置的内部记录不属于项目交付物，不将其作为项目文件的唯一保存位置。
- 文档、计划、调研及验收报告统一存入 `doc/`，使用单数目录名，不另建 `docs/`。
- Rust 源码归入 `src/`，集成测试及测试夹具归入 `tests/`，配置及配置样例归入 `config/`，开发辅助脚本归入 `scripts/`，部署文件归入 `deploy/`。
- 本地运行状态（session、关注名单等）归入 `data/`，文件日志归入 `logs/`，临时实验和下载文件归入 `tmp/`，锁和运行控制文件归入 `run/`，备份归入 `backups/`，构建产物归入 `target/`。按需创建目录，不预先生成空目录树。
- 根目录仅保留项目入口和工具要求的根文件，例如 `Cargo.toml`、`Cargo.lock`、`README.md`、`CLAUDE.md`、`.gitignore`，以及必要工具配置目录。
- 创建运行状态、临时文件或构建产物之前，为相应路径配置 `.gitignore`。配置样例可跟踪，实际凭据配置与运行状态不得跟踪。下载内容分别存入新的 `tmp/` 子目录，实验脚本与下载内容分开存放。
- 后续实现应显式设置文件路径，避免因工作目录、`TEMP`/`TMPDIR` 或 `CARGO_TARGET_DIR` 导致项目产物落到仓库之外。若工具强制要求外部位置，先说明限制，不自行改为外部项目存储。

## 单目录一键部署（强制）

- 发布包须支持一条安装命令部署到用户指定目录，无需目标机器安装 Rust 或单独部署数据库、缓存、Web 服务。不依赖 Docker、全局包安装或 root 权限完成默认安装。计划在 GitHub Actions 的 Ubuntu 24.04 runner 上构建 Linux x86_64/aarch64 发布包，目标须满足 glibc >= 2.39（如 Ubuntu 24.04、Debian 13），不可宣称适用于所有 Linux。在线引导默认固定 `v0.1.0`，不跟随 `latest`；只有 tag 发布工作流成功并上传资产后，才可声称已有发布包。
- 部署目录内统一包含 `bin/`（程序）、`config/`（配置及凭据）、`data/`（session、关注规则及数据库侧文件）、`logs/`、`tmp/`、`run/`（锁等运行控制文件）、`backups/`、`scripts/`、`doc/`。按需创建，可有子目录，但不得拆到多个根目录。
- 不把应用文件写到 `/etc`、`/var/lib`、`/var/log`、`/run`、`/tmp`、`~/.config` 等目录；不通过指向部署根外的符号链接变相拆分。应用写路径统一由同一个根目录解析，校验绝对路径、`..` 和符号链接逃逸；不得允许配置将数据路径单独指向外部。
- 发布布局由真实可执行文件 `<root>/bin/nestbot` 推导默认根目录，允许一个统一的显式 `--root` 参数；开发时从 `target/` 启动必须显式指定仓库根。所有启动、登录、管理和安装入口使用相同规则，不依赖当前工作目录。
- 临时文件、原子替换文件、SQLite WAL/SHM、日志轮转、下载解压、升级暂存及备份均留在根目录内；原子替换文件与目标文件放在同一目录。应用日志应有容量上限，默认写 `logs/`，不以 journald 作为日志存储。
- 默认由目录内脚本完成启动、状态检查和安全停止；管理进程时校验实例身份并使用单实例锁，不根据陈旧 PID 盲目终止进程。默认不注册 systemd 或开机启动，因为系统服务注册会在部署目录外产生文件。若用户另行明确选择系统集成，必须先说明例外及产生的位置，单独获批后再做。
- “一键部署”指自动完成本地安装与初始化，不代表跳过 Telegram 认证：首次仍需填写凭据、输入验证码/2FA、准备中转群。重装和升级必须保留已有配置、session 与规则；禁止清空目录式安装。先校验发布包平台和校验值，再在目录内暂存与替换程序；升级失败保留旧程序和数据。
- 停止实例后可整体迁移部署目录；目录内备份按清单包含程序版本、配置与状态，排除 `backups/`、`tmp/`、`run/` 和旧日志，避免递归备份自身。不可在数据库写入期间直接复制作为一致性备份。升级和回滚需考虑状态格式兼容，不只回滚二进制。暂时不支持不停机迁移。
- 单目录约束覆盖应用持有的本地文件，不意味着 Telegram 云端原消息及转发内容被下载保存，也不包括操作系统自身审计或管理工具内部记录。核心转发仍不下载媒体。

## Git 开发与 GitHub 署名（强制）

- 本项目使用 Git 辅助开发：修改前查看 `git status --short`，修改后检查 `git diff`；提交前检查 `git diff --cached`，仅暂存本次任务相关文件，不使用无差别暂存将既有删除带入提交。
- 只有用户明确要求时才创建提交或推送 GitHub。需要提交时，若当前在默认分支，先创建任务分支；不要擅自重写历史、丢弃未提交修改或强制推送。
- Git 提交作者和提交者仅使用项目所有者本人已配置的 Git 身份。提交前检查 `git var GIT_AUTHOR_IDENT` 和 `git var GIT_COMMITTER_IDENT`；身份缺失或异常时先询问，不伪造邮箱，不修改全局 Git 身份。
- 上传 GitHub 时，作者署名只有项目所有者本人，不添加 Claude、Anthropic 或其他 AI 工具作为作者、共同作者或提交者。
- 提交信息不得添加指向 Claude/AI 的 `Co-Authored-By`、`Signed-off-by` 或其他署名尾注；PR 标题、正文、发布说明及项目文档不添加 `Generated with Claude` 等 AI 生成署名或徽章。
- 上述署名约定适用于本项目后续新建的提交和发布内容，不授权修改历史提交，也不授权删除必须保留的第三方许可证或版权声明。

## 当前核心架构

- 单个 Rust 二进制、单进程：个人账号通过 MTProto 读取已加入源群的新消息；Bot 通过 HTTP Bot API 接收私聊管理命令并投递通知，不建立第二个 MTProto 客户端。
- 消息链路：按 `(源群 ID, 发言用户 ID)` 匹配 → 个人账号原生转发到私有中转超群 → 通过返回的中转消息 ID，让 Bot 原生转发到所有者私聊 → 附原消息链接提醒。
- 中转群关联在进程内完成，不依赖 Bot 接收中转群更新；原群与原消息 ID 在首次接收时保存，不能从转发署名反推原文位置。
- 配置计划放在 `config/config.toml`，动态关注名单放在 `data/watches.toml`，授权 session 放在 `data/`。关注名单写入成功后才确认命令；个人账号登录与常驻运行分为 `login`、`run` 两种模式。
- 首版只关注超级群中具有可识别个人 user_id 的实时新增消息；广播频道帖子、匿名管理员和以频道身份发言不支持按背后个人关注，添加不支持的源聊天或主体时明确拒绝。不得混用频道 ID 与用户 ID。不主动补停机历史，不同步编辑或删除，不下载媒体。管理命令仅允许所有者私聊执行；禁止转发的内容不通过复制或下载绕过限制。
- Bot 的转发、提醒、命令回复、失败提示和恢复告警统一经过有界出站调度器并共享冷却状态；429 时首版保守暂停该 Bot 全部出站发送，其他路径不能绕过。控制消息也计入总队列容量，告警合并，不能在限流期间无限积压。
- 低内存设计要求应用队列、并发、去重及规则集合有上限，同时核查 MTProto 库内部队列和 peer 缓存。应用队列有界不等于整个进程有界。过载允许有记录地丢弃，不以无界积压换取零丢失。
- `src/app.rs` 组合接收、过滤、命令与单一出站 worker；`src/telegram.rs` 直接消费底层有界原始更新，避开高层自动补差；`src/bot.rs` 持有跨会话共享 Bot 冷却。`src/paths.rs` 是所有文件路径与原子写的统一入口。
- 部署脚本首版仅支持新安装与同包幂等，拒绝不同版本覆盖，自动升级/备份/回滚仍是目标而非已实现能力。连接关闭事件在底层满队列时可能丢失，无法保证严格识别全部隐式重连；不得把“不主动补历史”扩大描述为所有断线场景均已验证。双次转发行为及内存指标仍需真实环境验证。

## 开发命令

从根目录使用 `scripts/cargo-local.ps1`（Windows PowerShell）或 `sh scripts/cargo-local.sh`（Linux），包装器将 CARGO_HOME、target 和 TEMP/TMP 定位到仓库内。不要直接运行会默认写全局缓存的 Cargo 命令。

```powershell
.\scripts\cargo-local.ps1 check
.\scripts\cargo-local.ps1 build --release
.\scripts\cargo-local.ps1 fmt --check
.\scripts\cargo-local.ps1 clippy --all-targets -- -D warnings
.\scripts\cargo-local.ps1 test
.\scripts\cargo-local.ps1 test <test_name_filter>
.\scripts\cargo-local.ps1 tree -e features
.\target\debug\nestbot.exe --root . init
.\target\debug\nestbot.exe --root . check
.\target\debug\nestbot.exe --root . login
.\target\debug\nestbot.exe --root . run
```

`<...>` 是占位符；login 需要交互且会访问真实 Telegram，仅在用户授权并准备好凭据时运行。`check` 仅本地校验。发布版默认以 bin 上一级作为根；开发二进制必须显式 --root。内存验收使用 Linux release RSS/PSS，不以 Windows debug 或二进制体积代替。
