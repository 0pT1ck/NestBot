use crate::{
    config::Config,
    domain::{JobPayload, TransferMode},
    interfaces::control::{self, Action},
    storage::{Store, vault::Vault},
    telegram::{Account, bounded, rpc},
};
use clap::{Parser, Subcommand, ValueEnum};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Parser)]
#[command(name = "nestbot", version, about = "归巢 · 低资源 Telegram 转存服务")]
pub struct Cli {
    #[arg(long, global = true, default_value = "config/nestbot.toml")]
    pub config: PathBuf,
    #[arg(long, global = true)]
    pub env_file: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Mode {
    Copy,
    Deep,
}
impl From<Mode> for TransferMode {
    fn from(value: Mode) -> Self {
        match value {
            Mode::Copy => Self::Copy,
            Mode::Deep => Self::Deep,
        }
    }
}

#[derive(Subcommand)]
pub enum Command {
    /// Create configuration and an encrypted local database.
    Init,
    /// Run Bot, Web, scheduler and local control service.
    Serve,
    /// Sign in to Telegram. Stop the service before login.
    Login {
        #[arg(long)]
        upload: bool,
    },
    /// Import old encrypted vaults and preferences; old files remain intact.
    ImportLegacy {
        path: PathBuf,
        #[arg(long, default_value = "LEGACY_VAULT_PASSWORD")]
        password_env: String,
    },
    /// Online database backup; save master.key separately too.
    Backup {
        path: PathBuf,
    },
    /// Check configuration without connecting to Telegram.
    Doctor,
    Status,
    Jobs {
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    Search {
        keyword: String,
        #[arg(long, default_value = "1")]
        pages: Option<u32>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        sort: Option<String>,
        #[arg(long)]
        resume: bool,
    },
    Fetch {
        keys: Vec<String>,
        #[arg(long="keys", num_args=1..)]
        direct_keys: Vec<String>,
        #[arg(long)]
        keyword: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        use_key: bool,
        #[arg(long)]
        no_caption: bool,
        #[arg(long)]
        tag_key: bool,
        #[arg(long, default_value_t = 1)]
        pages: u32,
        #[arg(long)]
        sort: Option<String>,
        #[arg(long, value_enum, default_value = "deep")]
        mode: Mode,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        batch: Option<String>,
        #[arg(long, default_value_t = 1)]
        start: u32,
        #[arg(long)]
        end: Option<u32>,
        #[arg(long)]
        redo: bool,
        #[arg(long)]
        dry_run: bool,
    },
    Stop {
        id: String,
    },
    Retry {
        id: String,
        /// Explicitly permit possible duplicate delivery after an uncertain send.
        #[arg(long)]
        allow_uncertain: bool,
    },
    Clear,
    Batches {
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    Entries {
        id: String,
        #[arg(long, default_value_t = 1)]
        start: u32,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    Set {
        #[arg(value_parser=["target","mode"])]
        name: String,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    Logs,
}

pub struct InstanceLock(std::fs::File);
impl InstanceLock {
    pub fn acquire(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|_| anyhow::anyhow!("service_already_running"))?;
        Ok(Self(file))
    }
}
impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

pub fn load_env_file(path: &Path) -> anyhow::Result<()> {
    // Called once in main before creating the Tokio runtime or spawning threads.
    let text = zeroize::Zeroizing::new(std::fs::read_to_string(path)?);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid_env_file"))?;
        let name = name.trim();
        anyhow::ensure!(
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !["HOME", "PATH", "RUST_LOG", "CARGO_HOME", "RUSTUP_HOME"].contains(&name),
            "invalid_env_file"
        );
        let value = value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if std::env::var_os(name).is_none() {
            unsafe {
                std::env::set_var(name, value);
            }
        }
    }
    Ok(())
}

pub async fn open_store(config: &Config) -> anyhow::Result<Store> {
    config.create_dirs()?;
    let password = Config::env_secret(&config.web.vault_password_env)
        .ok_or_else(|| anyhow::anyhow!("vault_password_required"))?;
    let path = config.paths.data.join("master.key");
    let vault = tokio::task::spawn_blocking(move || Vault::open(&path, &password)).await??;
    Store::open(
        &config.paths.data.join("nestbot.sqlite"),
        Arc::new(vault),
        config.limits.sqlite_cache_kib,
        config.limits.max_queue,
    )
}

async fn ask(label: &str, secret: bool) -> anyhow::Result<String> {
    let label = label.to_owned();
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        use std::io::Write;
        // Rust's stdout uses Unicode console output on Windows. rpassword's
        // default prompt writes UTF-8 bytes to CONOUT$, which may use a legacy code page.
        print!("{label}");
        std::io::stdout().flush()?;
        if secret {
            Ok(rpassword::read_password()?)
        } else {
            let mut value = String::new();
            std::io::stdin().read_line(&mut value)?;
            Ok(value.trim().into())
        }
    })
    .await?
}

async fn login(config: &Config, store: Store, upload: bool) -> anyhow::Result<()> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let account = bounded(
        &cancel,
        config.limits.request_timeout_secs,
        Account::connect(config, store, if upload { "upload" } else { "main" }),
    )
    .await?;
    if bounded(&cancel, config.limits.request_timeout_secs, async {
        account.client.is_authorized().await.map_err(rpc)
    })
    .await?
    {
        println!("账号已登录。");
        return Ok(());
    }
    let api_hash = Config::env_secret(&config.telegram.api_hash_env)
        .ok_or_else(|| anyhow::anyhow!("missing_api_credentials"))?;
    let phone = zeroize::Zeroizing::new(ask("手机号（含国家区号）：", false).await?);
    let token = bounded(&cancel, config.limits.request_timeout_secs, async {
        account
            .client
            .request_login_code(&phone, &api_hash)
            .await
            .map_err(rpc)
    })
    .await?;
    let code = zeroize::Zeroizing::new(ask("Telegram 验证码：", true).await?);
    let result = account.client.sign_in(&token, &code).await;
    match result {
        Ok(_) => {}
        Err(grammers_client::SignInError::PasswordRequired(token)) => {
            let password = zeroize::Zeroizing::new(ask("Telegram 两步验证密码：", true).await?);
            bounded(&cancel, config.limits.request_timeout_secs, async {
                account
                    .client
                    .check_password(token, password.as_bytes())
                    .await
                    .map_err(|_| anyhow::anyhow!("login_failed"))
            })
            .await?;
        }
        Err(_) => anyhow::bail!("login_failed"),
    }
    println!("登录成功，会话已加密保存。");
    Ok(())
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    if matches!(cli.command, Command::Init) && !cli.config.exists() {
        if let Some(parent) = cli.config.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &cli.config,
            include_str!("../../config/nestbot.example.toml"),
        )?;
    }
    let config = Config::load(&cli.config)?;
    if let Command::Backup { path } = &cli.command
        && config.paths.run.join("control.json").exists()
        && control::request(&config.paths.run, Action::Backup { path: path.clone() })
            .await
            .is_ok()
    {
        println!("在线数据库备份完成；请同时安全备份 master.key 和解锁密码。");
        return Ok(());
    }
    // A crashed service may leave discovery metadata. Offline operations still
    // acquire the exclusive data lock before opening the database.
    match cli.command {
        Command::Doctor => {
            println!(
                "配置有效。Web={}，白名单人数={}，队列上限={}，临时空间上限={} MiB",
                config.web.enabled,
                config.telegram.allowed_users.len(),
                config.limits.max_queue,
                config.limits.max_cache_bytes / 1024 / 1024
            );
            println!(
                "凭据状态：vault={} admin={} bot={} api_hash={}",
                Config::env_secret(&config.web.vault_password_env).is_some(),
                Config::env_secret(&config.web.password_env).is_some(),
                Config::env_secret(&config.telegram.bot_token_env).is_some(),
                Config::env_secret(&config.telegram.api_hash_env).is_some()
            );
            Ok(())
        }
        Command::Init
        | Command::Serve
        | Command::Login { .. }
        | Command::ImportLegacy { .. }
        | Command::Backup { .. } => {
            config.create_dirs()?;
            let _lock = InstanceLock::acquire(&config.paths.data.join("service.lock"))?;
            let store = open_store(&config).await?;
            match cli.command {
                Command::Init => {
                    println!(
                        "已初始化配置和加密数据库。设置 Telegram 配置及 NESTBOT_ADMIN_PASSWORD 后运行 nestbot serve。"
                    );
                    Ok(())
                }
                Command::Serve => crate::serve(config, store).await,
                Command::Login { upload } => login(&config, store, upload).await,
                Command::ImportLegacy { path, password_env } => {
                    let password = Config::env_secret(&password_env)
                        .or_else(|| Config::env_secret(&config.web.vault_password_env))
                        .ok_or_else(|| anyhow::anyhow!("vault_password_required"))?;
                    let (batches, entries) =
                        crate::application::migration::import_legacy(&store, &path, &password)
                            .await?;
                    println!(
                        "导入完成：{batches} 个批次，读取 {entries} 条。旧转存进度单独保留，未自动映射目标/模式；账号请重新登录。"
                    );
                    Ok(())
                }
                Command::Backup { path } => {
                    store.backup(&path).await?;
                    println!("数据库备份完成；请同时安全备份 master.key 和解锁密码。");
                    Ok(())
                }
                _ => unreachable!(),
            }
        }
        command => {
            let action = match command {
                Command::Status => Action::Status,
                Command::Jobs { limit, offset } => Action::Jobs { limit, offset },
                Command::Search {
                    keyword,
                    pages,
                    all,
                    sort,
                    resume,
                } => Action::Submit {
                    payload: JobPayload::Search {
                        keyword,
                        pages: if all { None } else { pages.map(|n| n.max(1)) },
                        sort,
                        resume,
                    },
                },
                Command::Fetch {
                    keys,
                    direct_keys,
                    keyword,
                    limit,
                    use_key,
                    no_caption,
                    tag_key,
                    pages,
                    sort,
                    mode,
                    target,
                    batch,
                    start,
                    end,
                    redo,
                    dry_run,
                } => {
                    let mut keys = keys;
                    keys.extend(direct_keys);
                    let keyword = keyword.or_else(|| {
                        if batch.is_none()
                            && keys.len() == 1
                            && !crate::interfaces::commands::looks_like_key(&keys[0])
                        {
                            Some(keys.remove(0))
                        } else {
                            None
                        }
                    });
                    let target = if let Some(target) = target {
                        target
                    } else {
                        control::request(
                            &config.paths.run,
                            Action::Preference {
                                name: "target".into(),
                                value: None,
                            },
                        )
                        .await?
                        .get("value")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&config.default_target)
                        .to_owned()
                    };
                    Action::Submit {
                        payload: JobPayload::Transfer {
                            options: crate::domain::TransferOptions {
                                keyword,
                                limit,
                                use_key,
                                keep_caption: !no_caption,
                                tag_key,
                                pages: Some(pages.max(1)),
                                sort,
                                ..Default::default()
                            },
                            keys,
                            batch,
                            start,
                            end,
                            mode: mode.into(),
                            target,
                            redo,
                            dry_run,
                        },
                    }
                }
                Command::Stop { id } => Action::Cancel { id },
                Command::Retry {
                    id,
                    allow_uncertain,
                } => Action::Retry {
                    id,
                    allow_uncertain,
                },
                Command::Clear => Action::Clear,
                Command::Batches { limit, offset } => Action::Batches { limit, offset },
                Command::Entries { id, start, limit } => Action::Entries { id, start, limit },
                Command::Set { name, value } => Action::Preference {
                    name,
                    value: Some(value),
                },
                Command::Logs => Action::Logs,
                _ => unreachable!(),
            };
            let data = control::request(&config.paths.run, action).await?;
            println!("{}", serde_json::to_string_pretty(&data)?);
            Ok(())
        }
    }
}
