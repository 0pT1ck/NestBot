use crate::paths::{Paths, read_limited};
use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;

const MAX_CONFIG_BYTES: usize = 64 * 1024;

/// Intentionally not Debug: credentials must never be included in diagnostics.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub api_id: i32,
    pub api_hash: String,
    pub bot_token: String,
    pub owner_id: i64,
    pub relay_chat_id: i64,
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
    #[serde(default = "default_max_rules")]
    pub max_rules: usize,
    #[serde(default = "default_task_ttl_secs")]
    pub task_ttl_secs: u64,
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.file("config/config.toml")?;
        let bytes = read_limited(&path, MAX_CONFIG_BYTES)?
            .context("config/config.toml is missing; create it before starting")?;
        let text =
            std::str::from_utf8(&bytes).map_err(|_| anyhow!("config/config.toml is not UTF-8"))?;
        // toml::de::Error displays the source line, which can contain a secret.
        // Deliberately do not attach it as an error source.
        let config: Self = toml::from_str(text).map_err(|_| {
            anyhow!("config/config.toml has invalid TOML or unsupported/missing fields")
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(self.api_id > 0, "api_id must be positive");
        ensure!(
            self.api_hash.len() == 32
                && self.api_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                && self.api_hash.bytes().any(|byte| byte != b'0'),
            "api_hash must be a nonzero 32-character hexadecimal credential"
        );
        let valid_token = self.bot_token.split_once(':').is_some_and(|(id, secret)| {
            !id.is_empty()
                && id.bytes().all(|byte| byte.is_ascii_digit())
                && id.parse::<i64>().is_ok_and(|id| id > 0)
                && secret.len() == 35
                && secret
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                && secret.bytes().any(|byte| byte != b'0')
        });
        ensure!(valid_token, "bot_token is not a valid Bot API token");
        ensure!(self.owner_id > 0, "owner_id must be a positive user ID");
        // Bot API channel IDs encode -(1_000_000_000_000 + raw_channel_id).
        // Require the requested -100... form, not merely any negative chat ID.
        ensure!(
            (-1_009_999_999_999..=-1_000_000_000_001).contains(&self.relay_chat_id),
            "relay_chat_id must be a Bot API supergroup ID in -100... format"
        );
        ensure!(
            (1..=1024).contains(&self.queue_capacity),
            "queue_capacity must be in 1..=1024"
        );
        ensure!(
            (1..=4096).contains(&self.max_rules),
            "max_rules must be in 1..=4096"
        );
        ensure!(
            (1..=300).contains(&self.task_ttl_secs),
            "task_ttl_secs must be in 1..=300"
        );
        Ok(())
    }
}

fn default_queue_capacity() -> usize {
    128
}

fn default_max_rules() -> usize {
    256
}

fn default_task_ttl_secs() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{atomic_write, test_support::TestRoot};

    const VALID: &str = "api_id = 12345\napi_hash = '0123456789abcdef0123456789abcdef'\nbot_token = '123456789:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi'\nowner_id = 123456789\nrelay_chat_id = -1001234567890\n";

    fn write_config(root: &TestRoot, text: &str) {
        atomic_write(&root.0.file("config/config.toml").unwrap(), text.as_bytes()).unwrap();
    }

    #[test]
    fn loads_defaults_and_valid_boundary_values() {
        let root = TestRoot::new("config-defaults");
        write_config(&root, VALID);
        let config = Config::load(&root.0).unwrap();
        assert_eq!(config.queue_capacity, 128);
        assert_eq!(config.max_rules, 256);
        assert_eq!(config.task_ttl_secs, 60);
        write_config(
            &root,
            &format!("{VALID}queue_capacity = 1024\nmax_rules = 4096\ntask_ttl_secs = 300\n"),
        );
        assert!(Config::load(&root.0).is_ok());
    }

    #[test]
    fn rejects_unknown_fields_and_out_of_range_limits() {
        let root = TestRoot::new("config-bounds");
        for setting in [
            "queue_capacity = 0",
            "queue_capacity = 1025",
            "max_rules = 0",
            "max_rules = 4097",
            "task_ttl_secs = 0",
            "task_ttl_secs = 301",
            "data_path = '/outside'",
        ] {
            write_config(&root, &format!("{VALID}{setting}\n"));
            assert!(Config::load(&root.0).is_err(), "accepted {setting}");
        }
    }

    #[test]
    fn rejects_invalid_credentials_and_id_domains() {
        let root = TestRoot::new("config-credentials");
        for (from, to) in [
            ("api_id = 12345", "api_id = 0"),
            ("api_id = 12345", "api_id = -1"),
            (
                "0123456789abcdef0123456789abcdef",
                "00000000000000000000000000000000",
            ),
            ("0123456789abcdef0123456789abcdef", "not-a-hash"),
            (
                "123456789:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi",
                "0:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi",
            ),
            (
                "123456789:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi",
                "123456789:bad",
            ),
            ("owner_id = 123456789", "owner_id = -123456789"),
            (
                "relay_chat_id = -1001234567890",
                "relay_chat_id = -1234567890",
            ),
            (
                "relay_chat_id = -1001234567890",
                "relay_chat_id = -1000000000000",
            ),
            (
                "relay_chat_id = -1001234567890",
                "relay_chat_id = -1001234567890123",
            ),
        ] {
            write_config(&root, &VALID.replace(from, to));
            assert!(Config::load(&root.0).is_err(), "accepted invalid {from}");
        }
    }

    #[test]
    fn missing_corrupt_oversized_and_non_utf8_configs_fail_without_secrets() {
        let root = TestRoot::new("config-errors");
        assert!(Config::load(&root.0).is_err());
        for text in [
            format!("{VALID}broken = ["),
            VALID.replace("bot_token = '", "bot_token = ['"),
        ] {
            write_config(&root, &text);
            let error = Config::load(&root.0).err().unwrap();
            let message = format!("{error:#}");
            assert!(!message.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
            assert!(!message.contains("0123456789abcdef"));
        }
        write_config(&root, &" ".repeat(MAX_CONFIG_BYTES + 1));
        assert!(Config::load(&root.0).is_err());
        atomic_write(&root.0.file("config/config.toml").unwrap(), &[0xff]).unwrap();
        assert!(Config::load(&root.0).is_err());
    }
}
