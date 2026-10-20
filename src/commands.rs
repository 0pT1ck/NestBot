use crate::pipeline::channel_id;
use anyhow::{Result, bail, ensure};

pub const HELP: &str = "关注提醒 Bot\n/groups [页码] 查看超级群（20条/页）\n/watch <群ID> <用户ID或@用户名> 添加关注\n/unwatch <群ID> <用户ID> 取消关注\n/list [页码] 查看关注名单\n/status 查看运行状态\n/help 帮助\n只支持超级群中可识别个人身份的新消息。";

#[derive(Debug, PartialEq)]
pub enum Command {
    Help,
    Groups(usize),
    Watch(i64, String),
    Unwatch(i64, i64),
    List(usize),
    Status,
}

pub fn parse(text: &str) -> Result<Command> {
    ensure!(text.len() <= 1024, "command too long");
    let parts: Vec<_> = text.split_whitespace().take(5).collect();
    let name = parts.first().copied().unwrap_or("");
    match name {
        "/help" | "/start" if parts.len() == 1 => Ok(Command::Help),
        "/status" if parts.len() == 1 => Ok(Command::Status),
        "/groups" | "/list" if parts.len() <= 2 => {
            let page = if let Some(p) = parts.get(1) {
                p.parse()?
            } else {
                1
            };
            ensure!((1..=100).contains(&page), "page must be between 1 and 100");
            if name == "/groups" {
                Ok(Command::Groups(page))
            } else {
                Ok(Command::List(page))
            }
        }
        "/watch" if parts.len() == 3 => {
            let chat = channel_id(parts[1])?;
            let user = parts[2];
            if let Some(name) = user.strip_prefix('@') {
                ensure!(
                    (5..=32).contains(&name.len())
                        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                    "invalid username"
                );
            } else {
                ensure!(user.parse::<i64>()? > 0, "user ID must be positive");
            }
            Ok(Command::Watch(chat, user.to_owned()))
        }
        "/unwatch" if parts.len() == 3 => {
            let user = parts[2].parse()?;
            ensure!(user > 0, "user ID must be positive");
            Ok(Command::Unwatch(channel_id(parts[1])?, user))
        }
        _ => bail!("命令格式错误，请发送 /help"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_commands_and_reject_excess_arguments() {
        assert_eq!(
            parse("/watch -1001234567890 42").unwrap(),
            Command::Watch(1234567890, "42".into())
        );
        assert_eq!(parse("/groups").unwrap(), Command::Groups(1));
        assert!(parse("/groups 0").is_err());
        assert!(parse("/watch 123 -1").is_err());
        assert!(parse("/status extra").is_err());
        assert!(parse("/watch 1 @not-valid").is_err());
    }
}
