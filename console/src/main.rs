//! The agent console: one web UI for many agents.
//!
//! ```text
//! browser ──► console   GET  /api/agents               every agent, with its /v1/info and health
//!                       GET  /api/agents/{id}/info     ─┐
//!                       POST /api/agents/{id}/chat     ─┼─► that agent's /v1/{info,chat,reset}
//!                       POST /api/agents/{id}/reset    ─┘
//! ```
//!
//! The browser only talks to the console, so agents need no public address
//! and no CORS. Chat streams (SSE) are passed through as they arrive.
//!
//! Agents come from `CONSOLE_AGENTS`: `id=url` pairs separated by commas,
//! e.g. `quokka=http://quokka:3000,trip-planner=http://trip-planner:3000`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::signal::unix::{SignalKind, signal};

/// Where the agents are. Phase 2 adds discovery of `Agent` resources in Kubernetes.
enum Registry {
    Static(Vec<AgentRef>),
}

#[derive(Clone)]
struct AgentRef {
    id: String,
    url: String,
}

impl Registry {
    fn from_env() -> anyhow::Result<Self> {
        let spec = std::env::var("CONSOLE_AGENTS").unwrap_or_default();
        Ok(Self::Static(parse_agents(&spec)?))
    }

    fn agents(&self) -> Vec<AgentRef> {
        match self {
            Self::Static(agents) => agents.clone(),
        }
    }

    fn get(&self, id: &str) -> Option<AgentRef> {
        self.agents().into_iter().find(|agent| agent.id == id)
    }
}

fn parse_agents(spec: &str) -> anyhow::Result<Vec<AgentRef>> {
    let mut agents: Vec<AgentRef> = Vec::new();
    for pair in spec.split(',').map(str::trim).filter(|pair| !pair.is_empty()) {
        let (id, url) = pair.split_once('=').with_context(|| format!("`{pair}` in CONSOLE_AGENTS is not id=url"))?;
        let (id, url) = (id.trim(), url.trim().trim_end_matches('/'));
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("agent id `{id}` may only have letters, digits, - and _");
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            bail!("the URL of agent `{id}` must start with http:// or https://");
        }
        if agents.iter().any(|agent| agent.id == id) {
            bail!("agent `{id}` is listed twice in CONSOLE_AGENTS");
        }
        agents.push(AgentRef { id: id.to_owned(), url: url.to_owned() });
    }
    Ok(agents)
}

struct AppState {
    registry: Registry,
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
async fn list_agents(State(state): State<Arc<AppState>>) -> Json<Vec<AgentStatus>> {
    let checks = state.registry.agents().into_iter().map(|agent| {
        let http = state.http.clone();
        async move {
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

/// Passes one request to the agent's `/v1/{operation}` and streams the answer back.
async fn forward(
    State(state): State<Arc<AppState>>,
    Path((id, operation)): Path<(String, String)>,
    method: Method,
    body: Bytes,
) -> Response {
    // Only the agent API; nothing else on the agent is reachable through the console.
    if !matches!((&method, operation.as_str()), (&Method::GET, "info") | (&Method::POST, "chat" | "reset")) {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no operation `{operation}`") }))).into_response();
    }
    let Some(agent) = state.registry.get(&id) else {
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

    let registry = Registry::from_env()?;
    let agents = registry.agents();
    if agents.is_empty() {
        tracing::warn!("no agents: set CONSOLE_AGENTS, e.g. quokka=http://127.0.0.1:3000");
    }
    for agent in &agents {
        tracing::info!("agent {} at {}", agent.id, agent.url);
    }

    let state = Arc::new(AppState {
        registry,
        http: reqwest::Client::builder().connect_timeout(Duration::from_secs(3)).build()?,
    });
    // The shared UI, switched to console mode.
    let page = include_str!("../../ui/index.html").replacen(r#"data-mode="agent""#, r#"data-mode="console""#, 1);
    let app = Router::new()
        .route("/", get(move || async move { Html(page) }))
        .route("/api/agents", get(list_agents))
        .route("/api/agents/{id}/info", get(forward_info))
        .route("/api/agents/{id}/{operation}", post(forward))
        .route("/healthz", get(|| async { "ok" }))
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

async fn forward_info(state: State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    forward(state, Path((id, "info".to_owned())), Method::GET, Bytes::new()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_agent_list() {
        let agents = parse_agents(" quokka=http://quokka:3000/ , trip-planner=http://trip:3000,").unwrap();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].url, "http://quokka:3000");
        assert_eq!(agents[1].id, "trip-planner");
        assert!(parse_agents("").unwrap().is_empty());
    }

    #[test]
    fn rejects_bad_entries() {
        assert!(parse_agents("quokka").is_err());
        assert!(parse_agents("a b=http://x").is_err());
        assert!(parse_agents("a=ftp://x").is_err());
        assert!(parse_agents("a=http://x,a=http://y").is_err());
    }
}
