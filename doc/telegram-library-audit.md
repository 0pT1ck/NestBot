# Telegram 0.11.0 适配与边界核查

## 范围与证据

本报告对应 `src/telegram.rs`；依赖精确固定为 `grammers-client`、`grammers-mtsender`、`grammers-session`、`grammers-tl-types` **0.11.0**。不能将旧版 0.7/0.8 的 `Client::connect`、`PackedChat`、`next_update` API 套用到本版本。

阅读的官方 crate 源码位置：

- `tmp/grammers-review/grammers-client-0.11.0/`：主流程已有的 client 源码。
- `tmp/grammers-mtsender-audit-011/grammers-mtsender-0.11.0/`：通过 crates.io 官方下载 API 单独下载、独立目录解包。
- `tmp/grammers-session-audit-011/grammers-session-0.11.0/`：同上。
- `tmp/cargo-home/registry/src/.../grammers-tl-types-0.11.0/tl/api.tl`：实际依赖中的 TL schema。

下载目录只阅读，未在这些目录执行构建或解释器；不 vendor 库，不执行下载内容。以下行号指原始 0.11.0 源码，不是本项目文件行号。

**验证状态：源码 API 核对已完成。主集成进程已报告编译成功及 27 个测试全部通过；首轮发现的极端 ID overflow 已修。之后追加了共享 FloodWait 冷却、relay 循环保护及 clippy 修正，最终构建/测试结果以主集成记录为准。本适配任务没有自行运行 cargo。没有 Telegram 真机凭据，尚未验证授权、真实超级群更新、双次原生转发、禁止转发错误、断网、FloodWait 或 Linux RSS/PSS。不能据此宣布生产验收通过。**

## 对外接口与主程序集成

```rust
pub struct Telegram(/* Arc-owned inner, Clone */);
pub struct Event {
    pub chat_id: i64,       // 裸正数 supergroup/channel ID，不是 -100... ID
    pub user_id: i64,       // 只取消息 from_id: Peer::User
    pub message_id: i32,
    pub date: i64,          // Telegram 消息 Unix 秒时间
    pub protected: bool,
    pub peer: grammers_session::types::PeerRef,
}

impl Telegram {
    pub async fn connect(config: &Config, paths: &Paths) -> anyhow::Result<Self>;
    pub async fn login(config: &Config, paths: &Paths) -> anyhow::Result<()>;
    pub async fn pump(&self, tx: tokio::sync::mpsc::Sender<Event>) -> anyhow::Result<()>;
    pub async fn close(&self);
    pub async fn forward(&self, source: PeerRef, message: i32) -> anyhow::Result<i32>;
    pub async fn delete_relay(&self, id: i32) -> anyhow::Result<()>;
    pub async fn groups(&self, page: usize) -> anyhow::Result<String>;
    pub async fn validate_group(&self, id: i64) -> anyhow::Result<PeerRef>;
    pub async fn resolve_user(&self, value: &str) -> anyhow::Result<i64>;
    pub fn forget_group(&self, bare_id: i64);
}
```

### 登录、启动与退出

- `SqliteSession::open(<root>/data/telegram.session).await`。不是 session 放内存最后再 save 的实现；SQLite storage 的修改方法直接执行数据库语句。
- `SenderPool::with_configuration(session, api_id, ConnectionParams { ... })` 拆出 `runner / handle / updates`；`Client::with_configuration(handle, ...)` 得到可 clone client。
- `connect` 不交互：检查 `is_authorized`、`get_me`、非 Bot 个人账号、账号裸 ID 等于 `owner_id`，再验证 relay 是已加入超级群。未授权直接报错，提示显式执行 login。
- `login` 才允许交互：电话、验证码和 2FA 密码均用 rpassword 隐藏终端输入；`request_login_code` → `sign_in` → `SignInError::PasswordRequired(token)` 时 `check_password(token, bytes)`。不打印密码提示 hint，不记录手机号、验证码、密码或 hash；这不代表 secret 字符串实现了内存零化。
- 先由公共路径层准备 private root/data，session 文件 UNIX 权限为 0600；同时检查已存在的 `-wal/-shm/-journal` 路径，拒绝目录或路径逃逸。SQLite 的具体临时文件行为仍须 Linux 真机核验。
- `pump` 消费唯一 receiver，不能调用两次。不替主程序 spawn pump，不创建每消息任务。
- 主程序持有并管理 pump/worker 任务，退出/重连时先取消并等待这些任务，再 `close().await`，最后丢弃全部克隆，之后才开启下一会话。不可对同一 SQLite session 同时开两个 SenderPool。
- `close` 发送 quit，等待 runner 最多 5 秒，超时 abort 并等待任务结束。Inner 最后 Drop 同样 abort runner；runner 内的 JoinSet Drop 会 abort 所有 DC sender 任务，避免 detached 永久后台任务。
- raw stream 不维护 MessageBoxes cursor，无高层 `sync_update_state` 可同步；授权/session 更新已经由 SQLite storage 写入。不要虚构“Drop 已同步全部更新游标”。

### 规则与用户解析

- 启动后必须对已保存的源群规则调用 `validate_group`，才能把该源加入有限源注册表并产生 Event。relay 在 connect 时加入。
- `validate_group` 接受裸正数 channel ID 或 Bot API 负数编码；逐项核对实际 dialog 中的 `Peer::Group` 和其 `Chat::Channel`，要求 `megagroup && !broadcast && !left && !min`。拒绝 basic group、广播频道、不可访问/退出群和新类型 community。不能凭 ID 的形状确认超级群。
- dialog 扫描最多 1000 个，每次服务器页最大 100；目标不在前 1000 个时明确失败，不承诺全账号搜索。这里未缓存整份 dialog 列表。
- `groups` 页码 **1 起算**，最多 50 页，每次最多输出 20 个超级群；每次最多扫描 1000 个 dialog。名字过滤控制字符并截断到 60 字符。
- 源注册表最多 `config.max_rules + 1` 项，项内只有 `PeerRef` 和 protected 布尔值。删除最后一条该源规则后主程序调用 `forget_group(bare_id)`，否则多轮新增/删除可能达到注册表上限；relay 不会被此方法移除。
- 数字 user ID 仅做正数/合法用户 ID 范围校验，不向服务器确认该 ID 的存在或身份；`@username` 调用 `resolve_username`，仅接受 `Peer::User`，拒绝群/频道。`Peer::User` 包括 Bot 用户，并不能宣称每个数字都是真人。
- 发现 `PeerId::channel(i64::MAX)` 内部使用 eager `then_some` 构造，debug 编译会发生加法 overflow；本项目先限制裸群 ID 范围，不把任意 i64 直接传进去。

### 新消息、保护与原生转发

- 直接消费 `SenderPool.updates: mpsc::Receiver<UpdatesLike>`，只展开 `Updates/Combined/UpdateShort` 中的 `NewChannelMessage`。
- 只接受普通 `Message`，排除 `Service/Empty`、outgoing、post、无正消息 ID、非 channel peer、没有 `from_id: Peer::User` 的消息。因此匿名管理员、频道身份发送、广播频道、service 和 basic group 不进入个人 user_id 匹配。
- 事件只引用已验证的超级群，并硬排除 relay 自身；forward 也拒绝 source 等于 relay，且 relay 初始化后 validate_group 拒绝再次把它作为 watched source，避免误加 `/watch relay` 保存无效规则或造成自身转发环。普通 channel 消息即使伪装成可匹配正数 ID，也不会绕过超级群注册检查。
- 群 `noforwards` 与消息 `noforwards` 合并为 Event.protected；上层必须丢弃 protected 命中。`forward` 另查已注册群的 protected 状态；服务器仍会最终执行 `CHAT_FORWARDS_RESTRICTED` 等限制。不复制正文、不下载媒体、不尝试规避保护。
- Event 仅含固定小字段，不保留整条消息、用户资料、媒体或 PeerMap；对有界 channel 只 `try_send`，满时丢弃新事件并汇总计数，心跳 tick 报告一次；不因下游限流 await 队列空位。
- `grammers-client/src/client/messages.rs:936–967` 的高层 `forward_messages` 硬编码 `silent: false`。本适配改用原生 `messages::ForwardMessages`，`silent: true`、保留作者与媒体 caption、禁止 paid flood bypass，目的地为已验证 relay。
- 转发请求中只传 source peer、单个原消息 ID、random_id、relay peer。响应中只接受匹配 random_id 的 `Update::MessageId`，返回 relay 消息 ID；不从署名猜原始来源，也不把原消息 ID 当 relay ID。
- 缺少 random_id 映射或请求 timeout 的结果可能“已发送但未得到确认”；返回错误且不盲重试。当前 random_id 基于时间与进程原子序列生成，同一进程严格递增；不提供跨进程持久化幂等键保证。
- `delete_relay` 以 relay PeerRef 调 `delete_messages`，库对 Channel 使用 `channels::DeleteMessages`，避免 basic-chat 账号级消息 ID 的误删语义。

## 内部队列、peer 内存与真正剩余边界

| 层 | 0.11.0 源码证据 | 本项目处理与剩余问题 |
| --- | --- | --- |
| 网络更新通道 | mtsender `sender_pool.rs:267–269`、`configuration.rs:59–63` | **已是 bounded**，默认 100，本适配显式设 64。满时 `try_send` 丢新 batch，不是无界 update queue。 |
| SenderPool 请求入口 | `sender_pool.rs:267` | **unbounded_channel**，官方公共配置没有容量参数。 |
| 每 DC RPC 队列 | `sender_pool.rs:394` | **unbounded_channel**。不能拿 app event queue 有界替代此事实。 |
| sender 等待请求 | `sender.rs:121,213` | `Vec<Request>`，没有显式条数上限。仅把 channel 换 bounded 还不等于 sender outstanding 有硬界。 |
| 单个 update batch | `UpdatesLike::Updates` 中 Vec | 容量 64 按 batch 计数，不是 64 条 message 或固定字节；单包反序列化仍有可变内存。 |
| 高层 UpdateStream | client `updates.rs:23–26,44–47` | VecDeque 保留整批及 PeerMap。本适配不走该层。 |
| MessageBoxes | session `message_box/*` | 跟踪频道状态、缺口和恢复；本适配不实例化，不随观察到的新频道积累状态。 |
| SqliteSession | session `storages/sqlite.rs:28–36,209–246` | 常驻 cache 只有 home_dc 与 dc_options，**没有全用户 peer_infos HashMap**。peer 查询走 SQLite；不能把 MemorySession 的 HashMap 误归给 SqliteSession。 |
| PeerMap | client `peer/peer_map.rs:25–28` | 每个响应一个 Arc<HashMap>，不是必然全局永久缓存。持有整个 Message 才可能延长整批 peer 生命周期；轻量 Event 不持有它。 |
| 默认自动 peer 缓存 | client `client/client.rs:38–52` | 本项目 `auto_cache_peers: false`；管理查询只临时构建响应 PeerMap，原生 update 不构建 PeerMap。账号自身登录信息仍由库写 session；dialog iterator 仍可能写频道 pts 状态到磁盘。 |

应用封装保证有限调用路径：管理、转发、删除共用一个 `rpc_gate`；pump 仅另有一个 Ping。公开封装没有暴露 Client 供外部任意并发 invoke，亦不为每条消息 spawn。因此正常集成下，在途 RPC 数有限。`NoRetries` 明确关闭库默认 FloodWait/IO 自动重试，不能让调用方误以为“库会无限帮我等”。

收到 RPC code 420 且带 seconds/value 时，记录所有应用调用共享的冷却截止时间；之后的转发、管理与 cleanup 请求在尚未 poll/入队前异步等待。原失败直接上报，不自动重试未知转发；多个等待取较晚截止时间。只有无内容的协议 Ping 绕过应用冷却，避免 update pump 因 FloodWait 睡眠数分钟。冷却截止时间为进程级 static 共享状态，符合本程序单账号假设，重建 Telegram/连接代不会清掉 FloodWait；未持久化到磁盘，进程重启不保留该状态，不可将重启当作规避服务端限流的手段。

每个实际执行的 RPC 内部 timeout 为 30 秒（不含冷却等待）；发生 timeout **销毁整个 pool generation**。增加 cancellation guard：外层 timeout/select 导致 RPC future 被丢弃时，同样 abort pool。否则取消 future 只是丢掉等待者，已经排队的 RPC 仍然可能积累。上层仍须限制自己创建的待锁任务数量；Mutex 等待者不是免费有界队列。

**结论：这是固定调用者条件下可控的实现，不是证明了整个库/进程内存硬上界。** 单 batch 字节数、sender request Vec、反序列化/传输缓冲、SQLite cache/临时空间以及依赖底层行为都要实测。需要严格硬界时，建议小范围上游补丁：请求入口和 per-DC 有界 admission、sender outstanding 总量限制、取消已关闭 oneshot 的请求、计数/溢出反馈；不能仅把一处 channel 替换掉后宣称完成。

## 实时边界与重连：不能隐藏的 blocker

### 为什么不用 `catch_up: false` 高层流

mtsender `updates_receiver.rs:92–101` 中 `catch_up: false` 仅让 `MessageBoxes` 初始为空。随后 `next()` 仍在 `167–224` 处理 `GetDifference/GetChannelDifference`。session `message_box/adaptor.rs:381` 把 `ConnectionClosed/MalformedUpdates` 当作 Gap。因此 **catch_up=false 不等于运行期间或重连期间绝不恢复缺口**。

本适配根本不建立 `UpdatesReceiver/UpdateStream/MessageBoxes`，不调用 `getDifference/getChannelDifference`，不读历史。代价是明确允许过载/断线丢失，不提供 ordered/gap-free 保证。主程序另外按每代启动时间 `date > boundary` 和任务 TTL 丢旧消息，并做有限去重；来源是“实时接收且时间过滤后的事件”，不是完整审计日志。

### 官方 public API 无法可靠检测每次断线

`sender_pool.rs:564–568`：home sender 断开时通过 **同一个 bounded 更新队列** `try_send(UpdatesLike::ConnectionClosed)`，忽略发送失败。队列满时，关闭通知可以丢失。之后 `process_request` 可按需为该 DC 建新连接；Ping 可能成功，**所以心跳不是可靠的断线 generation 观察接口**。

当前实现对可见 `ConnectionClosed`、MalformedUpdates、TooLong、update channel 关闭、Ping 失败/timeout 返回错误；上层应丢弃旧任务/队列，等待 5 秒，新建 pool 并重设 boundary。然而“每一次断线都被观察、所有旧任务都能在断线后必定清空”在未修改的 0.11.0 上 **不能保证**，静默重建连接可能漏掉重设边界。不得宣称完全实现严格重连实时语义。

最小可维护修复建议（尚未应用，不 vendor 大库）：

1. 在 SenderPoolRunner/connection 生命周期暴露独立 `watch` 状态或单调 generation，断线/重新建立时更新；不与 data queue 抢容量，也不只发一条可能丢失的事件。
2. 或增加可选 fail-closed 策略：home connection 第一次断线后整个 pool 停止，公共接口必定失败，不允许隐式新建。
3. 每个更新附上 generation、或者保证断线状态先被消费者看到，再允许发出新代更新；否则单独知道断线却不知道数据属于哪代仍有竞态。

在修复前，此项是严格验收的 blocker，而非“设置 catch_up=false 即已解决”。

## 后续必须执行的验收

1. 官方 0.11.0 固定版本编译、全量 unit tests、clippy；目前 unit tests 覆盖群 ID 域、极值拒绝、random_id 映射和序列唯一性，不代表网络集成测试。
2. 登录/2FA、owner 不匹配、未授权 run 不交互；重启读取同一个 SQLite session。
3. 加入真实超级群，验证 personal-user/匿名/channel身份/outgoing/service 的过滤；保护内容不会进入两次转发链路。
4. 实测 `messages.forwardMessages(silent=true)` 返回映射、Bot 基于 relay ID 二次原生转发、原始来源链接保留；验证转发成功但响应丢失时不盲重试。
5. MTProto FloodWait、短断网、长断网、满更新队列时断线通知丢失；明确观察未打补丁前的 silent-generation 缺陷。
6. 高频更新、稳定运行、反复规则增删、反复会话重建与取消 RPC；记录 Linux release RSS/PSS、FD、任务数、session 文件/侧文件大小与磁盘使用上限。不用 Windows debug 内存或二进制体积替代。
