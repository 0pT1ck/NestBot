use crate::{application::App, domain::JobPayload};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Status,
    Submit { payload: JobPayload },
    Jobs { limit: u32, offset: u32 },
    Cancel { id: String },
    Retry { id: String, allow_uncertain: bool },
    Clear,
    Batches { limit: u32, offset: u32 },
    Entries { id: String, start: u32, limit: u32 },
    Preference { name: String, value: Option<String> },
    Logs,
    Backup { path: std::path::PathBuf },
}

#[derive(Serialize, Deserialize)]
struct ControlInfo {
    address: String,
    token: String,
}
#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    #[serde(flatten)]
    action: Action,
}

pub async fn execute(app: &App, action: Action) -> anyhow::Result<serde_json::Value> {
    Ok(match action {
        Action::Status => {
            serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"jobs":app.store.jobs(10,0).await?,"target":app.target().await?,"mode":app.mode().await?,"limits":app.config.limits,"bot_configured":app.bot.is_some(),"web_enabled":app.config.web.enabled,"memory_rss_bytes":rss()})
        }
        Action::Submit { payload } => {
            serde_json::json!({"id":app.enqueue(payload,None,None).await?})
        }
        Action::Jobs { limit, offset } => {
            serde_json::to_value(app.store.jobs(limit, offset).await?)?
        }
        Action::Cancel { id } => {
            app.cancel(&id).await?;
            serde_json::json!({"ok":true})
        }
        Action::Retry {
            id,
            allow_uncertain,
        } => {
            app.store.retry(&id, allow_uncertain).await?;
            app.notify.notify_one();
            serde_json::json!({"ok":true})
        }
        Action::Clear => serde_json::json!({"cancelled":app.store.clear_queue().await?}),
        Action::Batches { limit, offset } => {
            serde_json::to_value(app.store.batches(limit, offset).await?)?
        }
        Action::Entries { id, start, limit } => {
            let entries = app.store.entries(&id, start, limit).await?;
            serde_json::json!({"keyword":app.store.batch_keyword(&id).await?,"entries":entries.into_iter().map(|(seq,entry)|serde_json::json!({"seq":seq,"entry":entry})).collect::<Vec<_>>()})
        }
        Action::Preference { name, value } => {
            anyhow::ensure!(
                ["target", "mode"].contains(&name.as_str()),
                "invalid_preference"
            );
            if let Some(value) = value {
                anyhow::ensure!(value.len() <= 256, "invalid_preference");
                if name == "mode" {
                    anyhow::ensure!(["copy", "deep"].contains(&value.as_str()), "invalid_mode");
                }
                app.store.set_preference(&name, &value).await?;
            }
            serde_json::json!({"value":app.store.preference(&name).await?})
        }
        Action::Logs => serde_json::to_value(app.store.jobs(50, 0).await?)?,
        Action::Backup { path } => {
            app.store.backup(&path).await?;
            serde_json::json!({"ok":true})
        }
    })
}

fn rss() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()?
            .lines()
            .find(|l| l.starts_with("VmRSS:"))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|n| n * 1024)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

async fn handle_stream<S>(stream: S, app: Arc<App>, token: Arc<String>) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    use tokio::io::AsyncReadExt;
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        (&mut reader)
            .take(256 * 1024 + 1)
            .read_until(b'\n', &mut line),
    )
    .await??;
    anyhow::ensure!(
        bytes > 0 && bytes <= 256 * 1024 && line.last() == Some(&b'\n'),
        "invalid_request"
    );
    let request: Request = serde_json::from_slice(&line)?;
    use subtle::ConstantTimeEq;
    anyhow::ensure!(
        bool::from(request.token.as_bytes().ct_eq(token.as_bytes())),
        "unauthorized"
    );
    let response = match execute(&app, request.action).await {
        Ok(data) => serde_json::json!({"ok":true,"data":data}),
        Err(error) => serde_json::json!({"ok":false,"error":crate::telemetry::safe_error(&error)}),
    };
    let mut body = serde_json::to_vec(&response)?;
    body.push(b'\n');
    tokio::time::timeout(Duration::from_secs(10), reader.get_mut().write_all(&body)).await??;
    Ok(())
}

pub async fn serve(app: Arc<App>) -> anyhow::Result<()> {
    let path = app.config.paths.run.join("control.json");
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("random_failed"))?;
    let token = Arc::new(hex::encode(bytes));
    #[cfg(unix)]
    let (listener, address) = {
        use std::os::unix::fs::PermissionsExt;
        let socket = app.config.paths.run.join("control.sock");
        if socket.exists() {
            std::fs::remove_file(&socket)?;
        }
        let listener = tokio::net::UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        (listener, socket.to_string_lossy().to_string())
    };
    #[cfg(not(unix))]
    let (listener, address) = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        (listener, address)
    };
    std::fs::write(
        &path,
        serde_json::to_vec(&ControlInfo {
            address,
            token: token.to_string(),
        })?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    let permits = Arc::new(tokio::sync::Semaphore::new(16));
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _=app.shutdown.cancelled()=>break,
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            stream=listener.accept()=>{
                let (stream,_)=stream?;
                if let Ok(permit)=permits.clone().try_acquire_owned() {let app=app.clone();let token=token.clone();tasks.spawn(async move {let _permit=permit;let _=handle_stream(stream,app,token).await;});}
            }
        }
    }
    tasks.abort_all();
    let _ = std::fs::remove_file(path);
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(app.config.paths.run.join("control.sock"));
    }
    Ok(())
}

pub async fn request(run: &Path, action: Action) -> anyhow::Result<serde_json::Value> {
    let info: ControlInfo = serde_json::from_slice(
        &std::fs::read(run.join("control.json"))
            .map_err(|_| anyhow::anyhow!("service_not_running"))?,
    )?;
    #[cfg(unix)]
    let stream = tokio::net::UnixStream::connect(&info.address).await?;
    #[cfg(not(unix))]
    let stream = tokio::net::TcpStream::connect(&info.address).await?;
    let mut stream = BufReader::new(stream);
    let mut body = serde_json::to_vec(&Request {
        token: info.token,
        action,
    })?;
    body.push(b'\n');
    stream.get_mut().write_all(&body).await?;
    let mut response = Vec::new();
    use tokio::io::AsyncReadExt;
    tokio::time::timeout(
        Duration::from_secs(30),
        stream
            .take(1024 * 1024 + 1)
            .read_until(b'\n', &mut response),
    )
    .await??;
    anyhow::ensure!(response.len() <= 1024 * 1024, "response_too_large");
    let response: serde_json::Value = serde_json::from_slice(&response)?;
    anyhow::ensure!(
        response.get("ok") == Some(&serde_json::Value::Bool(true)),
        "control_request_failed"
    );
    Ok(response.get("data").cloned().unwrap_or_default())
}
