# NestBot

GitHub：[0pT1ck/NestBot](https://github.com/0pT1ck/NestBot)

Rust 实时关注提醒服务：个人账号监听已加入的超级群，按群 ID 和个人用户 ID 过滤；通过私有中转超群，让 Bot 原生转发到自己的私聊，并附上原消息链接。

**当前为可编译的首版实现，尚未完成真实 Telegram 账号、Linux 部署和内存验收。** 不应视为已验收的生产服务。已知库行为限制见 [Telegram 适配审查](doc/telegram-library-audit.md)，部署能力及限制见 [部署说明](doc/deployment.md)。

## 功能

- 独立 `login` / `run`；个人账号 session 只保存在根目录 `data/`。
- 私聊 `/groups [页]`、`/watch <群ID> <用户ID或@用户名>`、`/unwatch <群ID> <用户ID>`、`/list [页]`、`/status`、`/help`；仅配置的所有者可操作。
- 原生服务端转发，不下载附件，不保存正文；受保护消息不绕过限制。
- 有界任务队列、限量去重、统一 Bot 出站限流、有限时效；过载会丢弃，发送结果不明时不盲重试。
- 配置、规则、授权、锁、日志及临时文件均在一个根目录。默认不安装系统服务，不写 `/etc` 或 `/var/lib`。

不支持基础群、广播频道作者识别、匿名管理员识别、历史补发、编辑/撤回同步和相册聚合。中转超群只加入自己与 Bot；建议开启自动删除作为失败清理兜底。

## 本地开发

需要 Rust 1.89 或以上及对应平台编译工具。请使用仓库包装脚本，避免依赖缓存、构建和临时文件写出仓库。

Windows PowerShell：

```powershell
.\scripts\cargo-local.ps1 test
.\scripts\cargo-local.ps1 clippy --all-targets -- -D warnings
.\scripts\cargo-local.ps1 fmt --check
.\scripts\cargo-local.ps1 build
.\target\debug\nestbot.exe --root . init
```

Linux：

```sh
sh scripts/cargo-local.sh test
sh scripts/cargo-local.sh build --release
./target/release/nestbot --root "$PWD" init
```

编辑 `config/config.toml`，填入 API ID/hash、Bot token、本人用户 ID、私有中转超群的 Bot API ID。实际配置被 Git 忽略；不要把凭据发到聊天或命令行参数中。`owner_id` 必须与登录的个人账号一致。

```sh
./target/release/nestbot --root "$PWD" check
./target/release/nestbot --root "$PWD" login
./target/release/nestbot --root "$PWD" run
```

Windows 使用对应 `.exe` 路径。先向自己的 Bot 发送 `/start`。`check` 仅检查本地配置与规则，不验证 Telegram 凭据或实际转发。`login` 需要交互输入手机、验证码及可能的两步验证密码。

另一个终端可使用 `status` / `stop`。`status` 退出码：0=实例持锁，3=未运行，1=错误；持锁不等于 Telegram 连接已就绪。`stop` 通过根内停止请求与实例锁工作，不按 PID 杀进程。

## Linux 单目录部署

计划通过 GitHub Actions 在推送 `v0.1.0` tag 后构建发布包；**只有发布工作流成功并生成 Release 资产后，以下在线安装入口才可使用**，此处不代表当前已有可下载的发布包。

发布平台为 **Linux x86_64 / aarch64，glibc >= 2.39**，使用 Ubuntu 24.04 runner 构建；目标系统可选 Ubuntu 24.04、Debian 13 或满足相同架构和运行库要求的系统。不是全 Linux 通用包，不支持 Alpine/musl 或 glibc 版本过低的系统。目标机无需 Rust 工具链。

计划的一行安装命令（引导脚本默认固定安装 `v0.1.0`，不跟随 `latest`）：

```sh
mkdir -p "$HOME/NestBot" && curl -fsSL https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/bootstrap.sh -o "$HOME/NestBot/.bootstrap.sh" && sh "$HOME/NestBot/.bootstrap.sh" --root "$HOME/NestBot"
```

上述命令从公开仓库获取并执行引导脚本，运行前应核验来源。引导下载、校验和暂存均留在所选根目录内；首次安装只完成本地初始化，**仍需填写 `$HOME/NestBot/config/config.toml` 并交互登录**：

```sh
"$HOME/NestBot/bin/nestbot" --root "$HOME/NestBot" check
"$HOME/NestBot/bin/nestbot" --root "$HOME/NestBot" login
sh "$HOME/NestBot/scripts/start.sh" --root "$HOME/NestBot"
```

也可在 Linux 构建 release 后运行 `sh scripts/package.sh` 生成 `target/` 内发布包，核验后解压到新的目标目录，再执行 `sh /path/to/NestBot/scripts/install.sh --root /path/to/NestBot`。平台要求、手动安装及维护限制见[部署说明](doc/deployment.md)。

首版安装器支持新安装和同包幂等运行，**拒绝不同版本直接覆盖；自动升级、回滚及备份命令尚未实现**。暂不自动开机启动或故障拉起。停机后可整体迁移目录。

## 运行边界

- Bot API 和 MTProto 是两套独立权限；Bot 不在源群时必须通过中转群。
- 原生转发署名不一定能回到群消息，因此额外提供独立的原消息按钮。私有链接仍要求本人有访问权限。
- 当前直接处理原始实时更新，不运行 `getDifference` 或历史扫描。可观察的断线会清空应用任务并重新建立连接；底层连接关闭通知在满队列时可能丢失，因此尚不能保证识别所有隐式重连，详见审查文档。
- 原生更新队列有界，RPC 的库内部队列仍有无界结构；应用使用固定调用者、串行请求门与取消即关闭连接控制积压，但尚未完成 RSS/PSS 长时测试，64 MiB 不是已实现保证。
- 连接建立失败/授权失效会退出，运行期更新通道失败会尝试重建；不自动重新登录。不保证零丢失或 exactly-once。

[实施计划](doc/implementation-plan.md)描述目标，实际支持能力以代码、本页和验收记录为准。
