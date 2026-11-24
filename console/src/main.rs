//! The agent console: one web UI for many agents.
//!
//! ```text
//! browser ──► console   GET  /api/agents               every agent, with its /v1/info and health
//!                       GET  /api/agents/{id}/info     ─┐
//!                       POST /api/agents/{id}/chat     ─┼─► that agent's /v1/{info,chat,reset}
//!                       POST /api/agents/{id}/reset    ─┘
//!                       GET  /api/me                   the signed-in user
//! ```
//!
//! The browser only talks to the console, so agents need no public address
//! and no CORS. Chat streams (SSE) are passed through as they arrive.
//!
//! Agents come from `CONSOLE_AGENTS` (`id=url` pairs separated by commas),
//! or with `CONSOLE_DISCOVERY=kubernetes` from the `Agent` resources in a
//! namespace (see `registry.rs`). `GET /api/agents/events` sends an event
//! whenever that list changes.
//!
//! With `OIDC_ISSUER` set, every `/api` request needs a valid user token
//! (from the oauth2-proxy sidecar), and each user sees only the agents their
//! `spec.access` allows; see `auth.rs`.

mod auth;
mod registry;

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::middleware::from_fn_with_state;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;

use auth::{Auth, User};
use registry::{AgentRef, Registry};

struct AppState {
    auth: Option<Arc<Auth>>,
    registry: Registry,
    changes: broadcast::Sender<()>,
    http: reqwest::Client,
}

#[derive(Serialize)]
struct AgentStatus {
    id: String,
    healthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    info: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Every agent with its info, asked in parallel. A slow or down agent
/// shows as unhealthy instead of holding up the list.
async fn list_agents(State(state): State<Arc<AppState>>, user: Option<Extension<User>>) -> Json<Vec<AgentStatus>> {
    let user = user.map(|Extension(user)| user);
    let visible = state.registry.agents().into_iter().filter(|agent| state.allows(user.as_ref(), agent));
    let checks = visible.map(|agent| {
        let http = state.http.clone();
        async move {
            // An Agent whose rollout is not done is listed, but not asked.
            if let Some(reason) = agent.not_ready {
                return AgentStatus { id: agent.id, healthy: false, info: None, error: Some(reason) };
            }
            let result = async {
                let response = http
                    .get(format!("{}/v1/info", agent.url))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await?
                    .error_for_status()?;
                response.json::<Value>().await
            }
            .await;
            match result {
                Ok(info) => AgentStatus { id: agent.id, healthy: true, info: Some(info), error: None },
                Err(error) => {
                    tracing::warn!("agent {} at {} is not reachable: {error}", agent.id, agent.url);
                    AgentStatus { id: agent.id, healthy: false, info: None, error: Some("not reachable".to_owned()) }
                }
            }
        }
    });
    Json(futures::future::join_all(checks).await)
}

impl AppState {
    /// Whether this user may see and use this agent. Open consoles allow everything.
    fn allows(&self, user: Option<&User>, agent: &AgentRef) -> bool {
        match (&self.auth, user) {
            (None, _) => true,
            (Some(auth), Some(user)) => auth.allows(user, agent.access.as_ref()),
            (Some(_), None) => false,
        }
    }
}

/// Who is signed in, and where to sign out. Open consoles have no user.
async fn me(State(state): State<Arc<AppState>>, user: Option<Extension<User>>) -> Json<Value> {
    match (&state.auth, user) {
        (Some(auth), Some(Extension(user))) => {
            Json(json!({ "user": user, "admin": auth.is_admin(&user), "sign_out": "oauth2/sign_out" }))
        }
        _ => Json(json!({ "user": null })),
    }
}

/// An event each time the list of agents changes, so the UI reloads it at once.
async fn agent_events(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let events = futures::stream::unfold(state.changes.subscribe(), |mut changes| async move {
        match changes.recv().await {
            // A lagging receiver still learns that something changed.
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                Some((Ok::<_, std::convert::Infallible>(Event::default().data("changed")), changes))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    });
    Sse::new(events).keep_alive(KeepAlive::default())
}

/// Passes one request to the agent's `/v1/{operation}` and streams the answer back.
async fn forward(
    State(state): State<Arc<AppState>>,
    user: Option<Extension<User>>,
    Path((id, operation)): Path<(String, String)>,
    method: Method,
    body: Bytes,
) -> Response {
    // Only the agent API; nothing else on the agent is reachable through the console.
    if !matches!((&method, operation.as_str()), (&Method::GET, "info") | (&Method::POST, "chat" | "reset")) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no operation `{operation}`") }))).into_response();
    }
    // An agent this user may not use looks the same as one that does not exist.
    let user = user.map(|Extension(user)| user);
    let Some(agent) = state.registry.get(&id).filter(|agent| state.allows(user.as_ref(), agent)) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no agent `{id}`") }))).into_response();
    };
    let mut request = state.http.request(method, format!("{}/v1/{operation}", agent.url));
    if !body.is_empty() {
        request = request.header(header::CONTENT_TYPE, "application/json").body(body);
    }
    // No overall timeout: a chat turn streams for as long as the agent works.
    // When the browser goes away, this stream is dropped, which ends the
    // agent's request too, as if the browser had called it directly.
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!("agent {id} at {}: {error}", agent.url);
            let error = format!("agent `{id}` is not reachable");
            return (StatusCode::BAD_GATEWAY, Json(json!({ "error": error }))).into_response();
        }
    };
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let mut proxied = Response::new(Body::from_stream(response.bytes_stream()));
    *proxied.status_mut() = status;
    if let Some(content_type) = content_type {
        proxied.headers_mut().insert(header::CONTENT_TYPE, content_type);
    }
    // Keeps reverse proxies (such as an ingress) from buffering the event stream.
    proxied.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
    proxied
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let (registry, changes) = Registry::from_env().await?;
    let agents = registry.agents();
    if agents.is_empty() && matches!(registry, Registry::Static(_)) {
        tracing::warn!("no agents: set CONSOLE_AGENTS, e.g. quokka=http://127.0.0.1:3000");
    }
    for agent in &agents {
        tracing::info!("agent {} at {}", agent.id, agent.url);
    }

    let auth = Auth::from_env(reqwest::Client::new());
    match &auth {
        Some(auth) => tracing::info!("sign-in required; tokens from {}", auth.issuer()),
        None => tracing::warn!("no OIDC_ISSUER: the console is open to anyone who can reach it"),
    }
    let state = Arc::new(AppState {
        auth: auth.clone(),
        registry,
        changes,
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            // No idle connections: Kubernetes balances a Service per connection, so a reused
            // connection would send every request to the same pod of an agent with several
            // replicas. A new connection per request lets each chat land on any pod; the
            // extra setup is nothing next to a model call.
            .pool_max_idle_per_host(0)
            .build()?,
    });
    // The shared UI, switched to console mode.
    let page = include_str!("../../ui/index.html").replacen(r#"data-mode="agent""#, r#"data-mode="console""#, 1);
    let api = Router::new()
        .route("/api/me", get(me))
        .route("/api/agents", get(list_agents))
        .route("/api/agents/events", get(agent_events))
        .route("/api/agents/{id}/info", get(forward_info))
        .route("/api/agents/{id}/{operation}", post(forward))
        .route_layer(from_fn_with_state(auth, auth::require_user));
    let app = Router::new()
        .route("/", get(move || async move { Html(page) }))
        .route("/healthz", get(|| async { "ok" }))
        .merge(api)
        .with_state(state);

    let addr = std::env::var("CONSOLE_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("agent console on http://{addr}");
    let mut terminate = signal(SignalKind::terminate())?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        })
        .await?;
    Ok(())
}

async fn forward_info(state: State<Arc<AppState>>, user: Option<Extension<User>>, Path(id): Path<String>) -> Response {
    forward(state, user, Path((id, "info".to_owned())), Method::GET, Bytes::new()).await
}
