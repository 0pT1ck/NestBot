use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::sleep;

const MAX_RESPONSE: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct Bot {
    client: reqwest::Client,
    base: String,
    owner: i64,
    relay: i64,
    cooldown: std::sync::Arc<std::sync::Mutex<Instant>>,
}

#[derive(Debug)]
pub enum ApiError {
    RateLimited(Duration),
    Rejected(i64),
    Unknown,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited(d) => write!(f, "Bot API rate limited for {} seconds", d.as_secs()),
            Self::Rejected(code) => write!(f, "Bot API rejected request ({code})"),
            Self::Unknown => write!(f, "Bot API outcome unknown; not retrying a possible send"),
        }
    }
}
impl std::error::Error for ApiError {}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    result: Option<Value>,
    error_code: Option<i64>,
    parameters: Option<Parameters>,
}
#[derive(Deserialize)]
struct Parameters {
    retry_after: Option<u64>,
}

#[derive(Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
}
#[derive(Deserialize)]
pub struct Message {
    pub from: Option<User>,
    pub chat: Chat,
    pub text: Option<String>,
}
#[derive(Deserialize)]
pub struct User {
    pub id: i64,
}
#[derive(Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: String,
}

impl Bot {
    pub fn new(token: &str, owner: i64, relay: i64) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(40))
                .pool_max_idle_per_host(2)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base: format!("https://api.telegram.org/bot{token}/"),
            owner,
            relay,
            cooldown: std::sync::Arc::new(std::sync::Mutex::new(Instant::now())),
        })
    }

    async fn call(&self, method: &str, body: Value) -> std::result::Result<Value, ApiError> {
        // Never expose reqwest errors: their URL contains the bot token.
        let mut response = self
            .client
            .post(format!("{}{method}", self.base))
            .json(&body)
            .send()
            .await
            .map_err(|_| ApiError::Unknown)?;
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(ApiError::Unknown);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ApiError::Unknown)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(ApiError::Unknown);
            }
            bytes.extend_from_slice(&chunk);
        }
        let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|_| ApiError::Unknown)?;
        if !envelope.ok {
            if let Some(seconds) = envelope.parameters.and_then(|p| p.retry_after) {
                let wait = Duration::from_secs(seconds.max(1));
                if let Some(until) = Instant::now().checked_add(wait) {
                    let mut cooldown = self.cooldown.lock().expect("cooldown lock");
                    *cooldown = (*cooldown).max(until);
                }
                return Err(ApiError::RateLimited(wait));
            }
            return Err(ApiError::Rejected(envelope.error_code.unwrap_or(0)));
        }
        envelope.result.ok_or(ApiError::Unknown)
    }

    pub async fn check(&self) -> Result<()> {
        let me = self.call("getMe", json!({})).await?;
        let id = me["id"].as_i64().context("getMe returned no bot ID")?;
        let relay = self.call("getChat", json!({"chat_id":self.relay})).await?;
        if relay["type"] != "supergroup" {
            bail!("relay_chat_id must identify a supergroup accessible to the bot");
        }
        let member = self
            .call("getChatMember", json!({"chat_id":self.relay,"user_id":id}))
            .await?;
        if matches!(member["status"].as_str(), Some("left" | "kicked")) {
            bail!("bot is not a member of the relay supergroup");
        }
        // Does not send a message; /start must still be performed by the owner.
        self.call("getChat", json!({"chat_id":self.owner})).await?;
        Ok(())
    }

    pub async fn updates(
        &self,
        offset: i64,
        timeout: u32,
        limit: u8,
    ) -> std::result::Result<Vec<Update>, ApiError> {
        let value = self
            .call(
                "getUpdates",
                json!({
                    "offset":offset,"timeout":timeout,"limit":limit,"allowed_updates":["message"]
                }),
            )
            .await?;
        serde_json::from_value(value).map_err(|_| ApiError::Unknown)
    }

    pub fn authorized_text<'a>(&self, update: &'a Update) -> Option<&'a str> {
        let m = update.message.as_ref()?;
        if m.chat.kind != "private" || m.chat.id != self.owner || m.from.as_ref()?.id != self.owner
        {
            return None;
        }
        m.text.as_deref().filter(|s| s.len() <= 1024)
    }

    pub async fn forward(&self, id: i32) -> std::result::Result<(), ApiError> {
        self.call(
            "forwardMessage",
            json!({
                "chat_id":self.owner,"from_chat_id":self.relay,"message_id":id,
                "disable_notification":true
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn text(&self, text: &str, link: Option<&str>) -> std::result::Result<(), ApiError> {
        let mut body =
            json!({"chat_id":self.owner,"text":text,"disable_notification":link.is_none()});
        if let Some(url) = link {
            body["reply_markup"] = json!({"inline_keyboard":[[{"text":"查看原消息","url":url}]]});
        }
        self.call("sendMessage", body).await?;
        Ok(())
    }
}

/// Owned by the single outbound worker. No other code sends Bot messages.
pub struct Sender {
    bot: Bot,
}
impl Sender {
    pub fn new(bot: Bot) -> Self {
        Self { bot }
    }

    pub async fn forward(&mut self, id: i32, deadline: Instant) -> Result<()> {
        self.send(Some(id), "", None, deadline).await
    }
    pub async fn text(&mut self, text: &str, link: Option<&str>, deadline: Instant) -> Result<()> {
        self.send(None, text, link, deadline).await
    }
    async fn send(
        &mut self,
        id: Option<i32>,
        text: &str,
        link: Option<&str>,
        deadline: Instant,
    ) -> Result<()> {
        for _ in 0..3 {
            loop {
                let cooldown = *self.bot.cooldown.lock().expect("cooldown lock");
                if cooldown >= deadline || Instant::now() >= deadline {
                    bail!("outbound task expired during cooldown");
                }
                if cooldown <= Instant::now() {
                    break;
                }
                sleep(cooldown.saturating_duration_since(Instant::now())).await;
            }
            let request = async {
                if let Some(id) = id {
                    self.bot.forward(id).await
                } else {
                    self.bot.text(text, link).await
                }
            };
            let result =
                tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), request)
                    .await
                    .map_err(|_| ApiError::Unknown)?;
            match result {
                Ok(()) => return Ok(()),
                Err(ApiError::RateLimited(_)) => {} // call() already extended the shared deadline.
                Err(e) => return Err(e.into()),
            }
        }
        bail!("Bot API retry budget exhausted")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn mock_bot(responses: Vec<&'static str>) -> (Bot, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buf = [0u8; 4096];
                    let count = stream.read(&mut buf).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..count]);
                    if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let length = header
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|s| s.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                stream.write_all(reply.as_bytes()).await.unwrap();
            }
            requests
        });
        let mut bot = Bot::new("123:abc", 42, -1000000000001).unwrap();
        bot.base = format!("http://{address}/");
        (bot, task)
    }

    #[tokio::test]
    async fn cooldown_survives_sender_recreation_and_blocks_other_paths() {
        let (bot, server) = mock_bot(vec![
            r#"{"ok":false,"error_code":429,"parameters":{"retry_after":30}}"#,
        ])
        .await;
        let mut sender = Sender::new(bot.clone());
        assert!(
            sender
                .forward(1, Instant::now() + Duration::from_secs(1))
                .await
                .is_err()
        );
        let mut replacement = Sender::new(bot);
        assert!(
            replacement
                .text("reply", None, Instant::now() + Duration::from_secs(1))
                .await
                .is_err()
        );
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn shorter_rate_limit_never_reduces_global_cooldown() {
        let (bot, server) = mock_bot(vec![
            r#"{"ok":false,"error_code":429,"parameters":{"retry_after":60}}"#,
            r#"{"ok":false,"error_code":429,"parameters":{"retry_after":1}}"#,
        ])
        .await;
        assert!(bot.forward(1).await.is_err());
        let longer = *bot.cooldown.lock().unwrap();
        assert!(bot.updates(0, 0, 1).await.is_err());
        assert!(*bot.cooldown.lock().unwrap() >= longer);
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn rejection_and_malformed_response_are_not_retried() {
        for body in [r#"{"ok":false,"error_code":403}"#, "not JSON"] {
            let (bot, server) = mock_bot(vec![body]).await;
            assert!(
                Sender::new(bot)
                    .forward(1, Instant::now() + Duration::from_secs(5))
                    .await
                    .is_err()
            );
            assert_eq!(server.await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn forwards_are_native_and_notification_contains_original_link() {
        let (bot, server) = mock_bot(vec![
            r#"{"ok":true,"result":{"message_id":7}}"#,
            r#"{"ok":true,"result":{"message_id":8}}"#,
        ])
        .await;
        let mut sender = Sender::new(bot);
        let deadline = Instant::now() + Duration::from_secs(5);
        sender.forward(123, deadline).await.unwrap();
        sender
            .text("matched", Some("https://t.me/c/99/88"), deadline)
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("POST /forwardMessage "));
        assert!(requests[0].contains("\"disable_notification\":true"));
        assert!(requests[1].contains("https://t.me/c/99/88"));
        assert!(requests[1].contains("\"disable_notification\":false"));
    }

    #[test]
    fn commands_require_owner_and_private_chat() {
        let bot = Bot::new("123:abc", 42, -1000000000001).unwrap();
        for (user, chat, kind, expected) in [
            (42, 42, "private", true),
            (41, 42, "private", false),
            (42, 42, "group", false),
            (42, 43, "private", false),
        ] {
            let update: Update = serde_json::from_value(json!({"update_id":1,"message":{"from":{"id":user},"chat":{"id":chat,"type":kind},"text":"/status"}})).unwrap();
            assert_eq!(bot.authorized_text(&update).is_some(), expected);
        }
    }
}
