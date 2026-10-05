use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub paths: Paths,
    pub telegram: TelegramConfig,
    pub web: WebConfig,
    pub limits: Limits,
    pub default_target: String,
    pub default_mode: crate::domain::TransferMode,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Paths {
    pub data: PathBuf,
    pub cache: PathBuf,
    pub run: PathBuf,
}
impl Default for Paths {
    fn default() -> Self {
        Self {
            data: ".local/data".into(),
            cache: ".local/cache".into(),
            run: ".local/run".into(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelegramConfig {
    pub api_id: i32,
    pub api_hash_env: String,
    pub bot_token_env: String,
    pub allowed_users: Vec<i64>,
    pub search_bot: String,
    pub file_bot: String,
    pub proxy_env: String,
}
impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            api_id: 0,
            api_hash_env: "TELEGRAM_API_HASH".into(),
            bot_token_env: "TELEGRAM_BOT_TOKEN".into(),
            allowed_users: vec![],
            search_bot: "example".into(),
            file_bot: "example".into(),
            proxy_env: "NESTBOT_PROXY".into(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub enabled: bool,
    pub listen: SocketAddr,
    pub password_env: String,
    pub vault_password_env: String,
    pub secure_cookie: bool,
    pub allow_remote: bool,
}
impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: "127.0.0.1:8787".parse().unwrap(),
            password_env: "NESTBOT_ADMIN_PASSWORD".into(),
            vault_password_env: "VAULT_PASSWORD".into(),
            secure_cookie: false,
            allow_remote: false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_queue: u32,
    pub max_cache_bytes: u64,
    pub reserve_disk_bytes: u64,
    pub max_file_bytes: u64,
    pub sqlite_cache_kib: u32,
    pub claim_timeout_secs: u64,
    pub transfer_stall_secs: u64,
    pub request_timeout_secs: u64,
    pub poll_secs: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_queue: 1024,
            max_cache_bytes: 5 * 1024 * 1024 * 1024,
            reserve_disk_bytes: 256 * 1024 * 1024,
            max_file_bytes: 4 * 1024 * 1024 * 1024,
            sqlite_cache_kib: 8192,
            claim_timeout_secs: 300,
            transfer_stall_secs: 60,
            request_timeout_secs: 90,
            poll_secs: 30,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(
            &std::fs::read_to_string(path).map_err(|_| anyhow::anyhow!("config_not_found"))?,
        )
        .map_err(|_| anyhow::anyhow!("invalid_config"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.web.listen.ip().is_loopback() || self.web.allow_remote,
            "remote_web_requires_opt_in"
        );
        anyhow::ensure!(
            !self.web.allow_remote || self.web.secure_cookie,
            "remote_web_requires_secure_cookie"
        );
        anyhow::ensure!(
            (1..=4096).contains(&self.limits.max_queue),
            "invalid_queue_limit"
        );
        anyhow::ensure!(
            (1024..=32768).contains(&self.limits.sqlite_cache_kib),
            "invalid_database_cache"
        );
        anyhow::ensure!(
            (10..=50).contains(&self.limits.poll_secs),
            "invalid_poll_timeout"
        );
        anyhow::ensure!(
            self.limits.claim_timeout_secs >= 10
                && self.limits.transfer_stall_secs >= 10
                && self.limits.request_timeout_secs >= 10,
            "invalid_timeout"
        );
        anyhow::ensure!(
            self.limits.max_cache_bytes >= self.limits.max_file_bytes,
            "cache_smaller_than_max_file"
        );
        Ok(())
    }

    pub fn create_dirs(&self) -> anyhow::Result<()> {
        for path in [&self.paths.data, &self.paths.cache, &self.paths.run] {
            std::fs::create_dir_all(path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }

    pub fn env_secret(name: &str) -> Option<zeroize::Zeroizing<String>> {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .map(zeroize::Zeroizing::new)
    }
}
