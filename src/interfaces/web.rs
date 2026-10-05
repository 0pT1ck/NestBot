use crate::{
    application::App,
    domain::unix_time,
    interfaces::control::{self, Action},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone)]
pub struct WebState {
    pub app: Arc<App>,
    auth: Arc<Auth>,
}
struct Session {
    expires: i64,
    csrf: String,
}
struct Auth {
    digest: [u8; 32],
    salt: [u8; 16],
    sessions: Mutex<HashMap<String, Session>>,
    attempts: Mutex<(i64, u32)>,
    login_gate: tokio::sync::Semaphore,
    request_gate: tokio::sync::Semaphore,
    streams: Arc<tokio::sync::Semaphore>,
}

fn derive(password: &str, salt: &[u8]) -> anyhow::Result<[u8; 32]> {
    let mut digest = [0u8; 32];
    let params =
        argon2::Params::new(32768, 3, 1, Some(32)).map_err(|_| anyhow::anyhow!("auth_failed"))?;
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut digest)
        .map_err(|_| anyhow::anyhow!("auth_failed"))?;
    Ok(digest)
}

impl WebState {
    pub async fn new(app: Arc<App>) -> anyhow::Result<Self> {
        let password = crate::config::Config::env_secret(&app.config.web.password_env)
            .ok_or_else(|| anyhow::anyhow!("admin_password_required"))?;
        Self::with_password(app, password).await
    }

    pub async fn with_password(
        app: Arc<App>,
        password: zeroize::Zeroizing<String>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(password.len() >= 12, "admin_password_too_short");
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|_| anyhow::anyhow!("random_failed"))?;
        let digest = tokio::task::spawn_blocking(move || derive(&password, &salt)).await??;
        Ok(Self {
            app,
            auth: Arc::new(Auth {
                digest,
                salt,
                sessions: Mutex::new(HashMap::new()),
                attempts: Mutex::new((0, 0)),
                login_gate: tokio::sync::Semaphore::new(1),
                request_gate: tokio::sync::Semaphore::new(32),
                streams: Arc::new(tokio::sync::Semaphore::new(8)),
            }),
        })
    }
}

fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(serde_json::json!({"error":code}))).into_response()
}

async fn security(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("content-security-policy","default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap());
    response
}

fn cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| part.trim().strip_prefix("nestbot_session="))
}

async fn auth(State(state): State<WebState>, request: Request, next: Next) -> Response {
    let Ok(_permit) = state.auth.request_gate.try_acquire() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "server_busy");
    };
    {
        let mut sessions = match state.auth.sessions.lock() {
            Ok(s) => s,
            Err(_) => return error(StatusCode::SERVICE_UNAVAILABLE, "auth_unavailable"),
        };
        sessions.retain(|_, session| session.expires > unix_time());
        let Some(session) = cookie(request.headers()).and_then(|token| sessions.get(token)) else {
            return error(StatusCode::UNAUTHORIZED, "login_required");
        };
        if request.method() != axum::http::Method::GET {
            use subtle::ConstantTimeEq;
            let csrf = request
                .headers()
                .get("x-csrf-token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !bool::from(csrf.as_bytes().ct_eq(session.csrf.as_bytes())) {
                return error(StatusCode::FORBIDDEN, "invalid_csrf");
            }
            if request
                .headers()
                .get("sec-fetch-site")
                .is_some_and(|v| v == "cross-site")
            {
                return error(StatusCode::FORBIDDEN, "invalid_origin");
            }
        }
    }
    next.run(request).await
}

#[derive(Deserialize)]
struct Login {
    password: String,
}
async fn login(State(state): State<WebState>, Json(login): Json<Login>) -> Response {
    if login.password.len() > 1024 {
        return error(StatusCode::BAD_REQUEST, "invalid_login");
    }
    {
        let mut attempts = state.auth.attempts.lock().unwrap();
        if unix_time() - attempts.0 >= 60 {
            *attempts = (unix_time(), 0);
        }
        if attempts.1 >= 5 {
            return error(StatusCode::TOO_MANY_REQUESTS, "login_rate_limited");
        }
        attempts.1 += 1;
    }
    let Ok(_permit) = state.auth.login_gate.try_acquire() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "login_busy");
    };
    let salt = state.auth.salt;
    let password = zeroize::Zeroizing::new(login.password);
    let derived = tokio::task::spawn_blocking(move || derive(&password, &salt)).await;
    use subtle::ConstantTimeEq;
    if !matches!(derived,Ok(Ok(ref digest)) if bool::from(digest.ct_eq(&state.auth.digest))) {
        return error(StatusCode::UNAUTHORIZED, "invalid_login");
    }
    let mut token = [0u8; 32];
    let mut csrf = [0u8; 32];
    if getrandom::fill(&mut token).is_err() || getrandom::fill(&mut csrf).is_err() {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "auth_failed");
    }
    let token = hex::encode(token);
    let csrf = hex::encode(csrf);
    let mut sessions = state.auth.sessions.lock().unwrap();
    sessions.retain(|_, s| s.expires > unix_time());
    if sessions.len() >= 16 {
        return error(StatusCode::TOO_MANY_REQUESTS, "too_many_sessions");
    }
    sessions.insert(
        token.clone(),
        Session {
            expires: unix_time() + 8 * 3600,
            csrf: csrf.clone(),
        },
    );
    let mut response = Json(serde_json::json!({"csrf":csrf})).into_response();
    let cookie = format!(
        "nestbot_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=28800{}",
        if state.app.config.web.secure_cookie {
            "; Secure"
        } else {
            ""
        }
    );
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie.parse().unwrap());
    response
}

async fn session_info(State(state): State<WebState>, headers: HeaderMap) -> Response {
    let sessions = state.auth.sessions.lock().unwrap();
    match cookie(&headers).and_then(|token| sessions.get(token)) {
        Some(session) => Json(serde_json::json!({"csrf":session.csrf})).into_response(),
        None => error(StatusCode::UNAUTHORIZED, "login_required"),
    }
}
async fn logout(State(state): State<WebState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie(&headers) {
        state.auth.sessions.lock().unwrap().remove(token);
    }
    (
        [(
            header::SET_COOKIE,
            "nestbot_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        )],
        Json(serde_json::json!({"ok":true})),
    )
        .into_response()
}

async fn action_response(state: &WebState, action: Action) -> Response {
    match control::execute(&state.app, action).await {
        Ok(value) => Json(value).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, crate::telemetry::safe_error(&e)),
    }
}
#[derive(Deserialize, Default)]
struct Pagination {
    #[serde(default)]
    offset: u32,
    #[serde(default = "page_limit")]
    limit: u32,
}
fn page_limit() -> u32 {
    50
}
async fn status(State(s): State<WebState>) -> Response {
    action_response(&s, Action::Status).await
}
async fn jobs(State(s): State<WebState>, Query(p): Query<Pagination>) -> Response {
    action_response(
        &s,
        Action::Jobs {
            limit: p.limit,
            offset: p.offset,
        },
    )
    .await
}
async fn submit(
    State(s): State<WebState>,
    Json(payload): Json<crate::domain::JobPayload>,
) -> Response {
    action_response(&s, Action::Submit { payload }).await
}
async fn cancel(State(s): State<WebState>, Path(id): Path<String>) -> Response {
    action_response(&s, Action::Cancel { id }).await
}
#[derive(Deserialize)]
struct Retry {
    #[serde(default)]
    allow_uncertain: bool,
}
async fn retry(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Json(request): Json<Retry>,
) -> Response {
    action_response(
        &s,
        Action::Retry {
            id,
            allow_uncertain: request.allow_uncertain,
        },
    )
    .await
}
async fn clear(State(s): State<WebState>) -> Response {
    action_response(&s, Action::Clear).await
}
async fn batches(State(s): State<WebState>, Query(p): Query<Pagination>) -> Response {
    action_response(
        &s,
        Action::Batches {
            limit: p.limit,
            offset: p.offset,
        },
    )
    .await
}
async fn entries(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Query(p): Query<Pagination>,
) -> Response {
    action_response(
        &s,
        Action::Entries {
            id,
            start: p.offset + 1,
            limit: p.limit,
        },
    )
    .await
}
#[derive(Deserialize)]
struct Preference {
    name: String,
    value: String,
}
async fn preference(State(s): State<WebState>, Json(p): Json<Preference>) -> Response {
    action_response(
        &s,
        Action::Preference {
            name: p.name,
            value: Some(p.value),
        },
    )
    .await
}
async fn logs(State(s): State<WebState>) -> Response {
    action_response(&s, Action::Logs).await
}
async fn events(State(s): State<WebState>, headers: HeaderMap) -> Response {
    let Ok(permit) = s.auth.streams.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "too_many_streams");
    };
    let token = cookie(&headers).unwrap_or("").to_owned();
    let stream = futures_util::stream::unfold(
        (s.app.events.subscribe(), permit, token, s.auth),
        |(mut rx, permit, token, auth)| async move {
            loop {
                let valid = auth.sessions.lock().ok().is_some_and(|sessions| {
                    sessions
                        .get(&token)
                        .is_some_and(|session| session.expires > unix_time())
                });
                if !valid {
                    return None;
                }
                let event = match tokio::time::timeout(Duration::from_secs(15), rx.recv()).await {
                    Ok(Ok(event)) => Event::default().event("job").json_data(event).ok(),
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                        Some(Event::default().event("resync").data("{}"))
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return None,
                    Err(_) => continue,
                };
                if let Some(event) = event {
                    return Some((
                        Ok::<_, std::convert::Infallible>(event),
                        (rx, permit, token, auth),
                    ));
                }
            }
        },
    );
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

pub fn router(state: WebState) -> Router {
    let private = Router::new()
        .route("/api/session", get(session_info))
        .route("/api/logout", post(logout))
        .route("/api/status", get(status))
        .route("/api/jobs", get(jobs).post(submit))
        .route("/api/jobs/{id}/cancel", post(cancel))
        .route("/api/jobs/{id}/retry", post(retry))
        .route("/api/queue/clear", post(clear))
        .route("/api/batches", get(batches))
        .route("/api/batches/{id}/entries", get(entries))
        .route("/api/preferences", post(preference))
        .route("/api/logs", get(logs))
        .route("/api/events", get(events))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));
    Router::new()
        .merge(private)
        .route("/api/login", post(login))
        .route(
            "/",
            get(|| async { Html(include_str!("../../web/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../../web/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../../web/style.css"),
                )
            }),
        )
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(middleware::from_fn(security))
        .with_state(state)
}

pub async fn serve(state: WebState) -> anyhow::Result<()> {
    let app = state.app.clone();
    let listener = tokio::net::TcpListener::bind(app.config.web.listen).await?;
    tracing::info!(event="web_started",address=%app.config.web.listen);
    axum::serve(listener, router(state))
        .with_graceful_shutdown(app.shutdown.clone().cancelled_owned())
        .await?;
    Ok(())
}
