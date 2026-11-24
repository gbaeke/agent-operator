//! One generic agent binary. Everything that makes an agent unique (model,
//! instructions, skills, knowledge, tools, subagents) comes from an agent
//! folder (see `config.rs`); deployment settings come from the environment.
//!
//! ```text
//! agent [AGENT_DIR] [--check]
//! ```
//!
//! The folder is the argument, else `$AGENT_DIR`, else the working directory.
//! `--check` loads and validates it, prints what the agent would get, and exits.
//!
//! Identity: a request may carry the user's token (`Authorization: Bearer`,
//! sent by the console when sign-in is on). The runtime then connects to its
//! MCP servers for that turn with the user's token, so every tool call is made
//! as the user, and the session belongs to that user (the token's `sub`).
//! Without a token the MCP connections made at startup are shared, as before.

mod config;
mod events;
mod kb;
mod skills;
mod store;
mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use rig::DynModel;
use rig::agent::{Agent, MultiTurnStreamItem};
use rig::catalog::Catalog;
use rig::message::ToolResultContent;
use rig::operation::Completion;
use rig::prelude::*;
use rig::providers::registry::{ConnectError, ConnectOptions};
use rig::streaming::{Item, StreamEvent};
use rig::tool::DynamicTool;
use rig::tool::rmcp::McpConnection;
use rmcp::ServiceExt;
use rmcp::model::{ClientCapabilities, ClientConfig, Implementation, ProtocolVersion};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde::Deserialize;
use serde_json::json;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;

use config::{Builtin, Bundle, McpServer};
use events::{EventSender, UiEvent};
use store::{History, Store};
use tools::*;

const MAX_TOOL_RESULT_CHARS: usize = 4_000;

struct AppState {
    bundle: Bundle,
    system_prompt: String,
    model: DynModel<Completion>,
    knowledge: Option<Arc<kb::KnowledgeBase>>,
    skills: Option<Arc<Vec<skills::Skill>>>,
    subagents: Arc<Vec<Subagent>>,
    /// Connected at startup without a user, for requests that carry no token.
    shared_mcp_tools: Vec<DynamicTool>,
    /// The MCP tool names last seen, for `/v1/info`.
    mcp_tool_names: std::sync::RwLock<Vec<String>>,
    store: Store,
}

impl AppState {
    /// The main agent for one run, its tools bound to the session's workspace
    /// and its MCP tools connected for this run's user.
    fn agent(&self, workspace: &SharedWorkspace, events: EventSender, mcp: Vec<DynamicTool>) -> Agent {
        let bundle = &self.bundle;
        let mut builder = AgentBuilder::new(self.model.clone())
            .preamble(&self.system_prompt)
            .max_tokens(bundle.config.max_tokens);
        if let Some(temperature) = bundle.config.temperature {
            builder = builder.temperature(temperature);
        }
        if bundle.enabled(Builtin::Planning) {
            builder = builder.tool(WriteTodos { workspace: workspace.clone() });
        }
        if bundle.enabled(Builtin::Files) {
            builder = builder
                .tool(ListFiles { workspace: workspace.clone() })
                .tool(ReadFile { workspace: workspace.clone() })
                .tool(WriteFile { workspace: workspace.clone() })
                .tool(EditFile { workspace: workspace.clone() });
        }
        if let Some(kb) = &self.knowledge {
            builder = builder
                .tool(SearchKnowledgeBase { kb: kb.clone() })
                .tool(ReadDocument { kb: kb.clone() })
                .tool(ListDocuments { kb: kb.clone() });
        }
        if let Some(skills) = &self.skills {
            builder = builder.tool(LoadSkill { skills: skills.clone() });
        }
        if !self.subagents.is_empty() {
            builder = builder.tool(Task {
                subagents: self.subagents.clone(),
                tools: SubagentTools {
                    knowledge: self.knowledge.clone(),
                    skills: self.skills.clone(),
                    mcp: mcp.clone(),
                },
                events,
            });
        }
        builder.dynamic_tools(mcp).build()
    }

    /// The MCP tools for one turn: connected with the user's token when there
    /// is one, so every call is made as that user; else the shared ones.
    async fn mcp_tools_for(&self, token: Option<&str>) -> Vec<DynamicTool> {
        let Some(token) = token else {
            return self.shared_mcp_tools.clone();
        };
        let servers = &self.bundle.config.mcp_servers;
        let tools: Vec<DynamicTool> =
            futures::future::join_all(servers.iter().map(|server| user_mcp_tools(server, token))).await.into_iter().flatten().collect();
        *self.mcp_tool_names.write().unwrap_or_else(std::sync::PoisonError::into_inner) =
            tools.iter().map(|tool| tool.name().to_string()).collect();
        tools
    }
}

/// The bearer token of a request, if it has one.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ").filter(|token| !token.is_empty())
}

/// The user a token names (its `sub`). The token is not checked here: only the
/// console, which checks it, can reach an agent (network policy).
fn subject(token: &str) -> Option<String> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("sub")?.as_str().map(str::to_owned)
}

/// A completion model from a `vendor/model` reference, keyed from the
/// environment. Behind a gateway (`base_url`) the key is optional: the
/// gateway usually holds the real one, so a placeholder is sent instead.
fn connect_model(model: &str, base_url: Option<&str>) -> anyhow::Result<DynModel<Completion>> {
    let catalog = Catalog::builtin();
    let Some(url) = base_url else {
        return catalog.connect(model).with_context(|| format!("model `{model}`"));
    };
    match catalog.connect_with(model, ConnectOptions::new().base_url(url)) {
        Err(ConnectError::MissingKey { .. }) => {
            catalog.connect_with(model, ConnectOptions::new().base_url(url).api_key("gateway-holds-the-key"))
        }
        result => result,
    }
    .with_context(|| format!("model `{model}` via {url}"))
}

/// Connects to an MCP server, as the user when a token is given, and lists its tools.
async fn connect_mcp(url: &str, token: Option<&str>) -> anyhow::Result<Vec<DynamicTool>> {
    // The newest protocol version with `initialize`: rmcp offers a newer one
    // by default, which agentgateway refuses on an `initialize` request.
    let config = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("agent-runtime", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE);
    let mut transport = StreamableHttpClientTransportConfig::with_uri(url);
    if let Some(token) = token {
        transport = transport.auth_header(token);
    }
    let transport = StreamableHttpClientTransport::from_config(transport);
    // The tools hold the connection open for as long as they live.
    let connection = McpConnection::new(config.serve(transport).await?);
    connection
        .tools()
        .await?
        .into_iter()
        .map(|tool| DynamicTool::try_from(tool).map_err(|error| anyhow::anyhow!("{error}")))
        .collect()
}

/// One server's tools, retried for a few seconds because in containers the
/// servers start together. An unreachable server is logged and skipped.
async fn mcp_tools(server: &McpServer) -> Vec<DynamicTool> {
    let url = &server.url;
    for attempt in 1..=10 {
        // Capped: a connect to a pod that is going away can otherwise hang for 30s.
        let connect = tokio::time::timeout(std::time::Duration::from_secs(2), connect_mcp(url, None));
        match connect.await.unwrap_or_else(|_| Err(anyhow::anyhow!("timed out"))) {
            Ok(tools) => {
                let tools = allowed(server, tools);
                tracing::info!("MCP server {url}: {} tools", tools.len());
                return tools;
            }
            Err(error) if attempt == 10 => tracing::warn!("no tools from MCP server {url}: {error}"),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
    Vec::new()
}

/// One server's tools for one turn, connected with the user's token. A
/// server that fails is left out of the turn, not replaced by the shared
/// connection: tools must not run as nobody when a user is known.
async fn user_mcp_tools(server: &McpServer, token: &str) -> Vec<DynamicTool> {
    let connect = tokio::time::timeout(std::time::Duration::from_secs(5), connect_mcp(&server.url, Some(token)));
    match connect.await.unwrap_or_else(|_| Err(anyhow::anyhow!("timed out"))) {
        Ok(tools) => allowed(server, tools),
        Err(error) => {
            tracing::warn!("no tools from MCP server {} for this turn: {error}", server.url);
            Vec::new()
        }
    }
}

/// Only the tools the server's allowlist names, if it has one.
fn allowed(server: &McpServer, mut tools: Vec<DynamicTool>) -> Vec<DynamicTool> {
    if let Some(allowed) = &server.tools {
        tools.retain(|tool| allowed.iter().any(|name| name == tool.name()));
    }
    tools
}

#[derive(Deserialize)]
struct ChatRequest {
    session_id: String,
    message: String,
}

#[derive(Deserialize)]
struct SessionRequest {
    session_id: String,
}

fn internal_error(error: anyhow::Error) -> Response {
    tracing::error!("{error:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": error.to_string() }))).into_response()
}

/// Runs the agent on one message and streams what happens as SSE events.
/// The history is loaded before the first model call and saved when the turn
/// succeeds; the workspace is saved after every tool result.
async fn chat(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(request): Json<ChatRequest>) -> Response {
    let session_id = request.session_id.clone();
    let token = bearer(&headers);
    // The session is the user's: a new one becomes theirs, someone else's is refused.
    match state.store.touch(&session_id, token.and_then(subject).as_deref()).await {
        Ok(true) => {}
        Ok(false) => return not_yours(),
        Err(error) => return internal_error(error),
    }
    // One turn per session at a time, also across replicas: two turns would
    // edit the same plan and files, and the second history save would be refused.
    let lock = match state.store.lock(&session_id).await {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            let error = "This chat is still working on a previous message.";
            return (StatusCode::CONFLICT, Json(json!({ "error": error }))).into_response();
        }
        Err(error) => return internal_error(error),
    };
    let workspace = match state.store.load_workspace(&session_id).await {
        Ok(workspace) => SharedWorkspace::new(workspace),
        Err(error) => return internal_error(error),
    };
    let (sender, mut subagent_events) = mpsc::unbounded_channel();
    let mcp = state.mcp_tools_for(token).await;
    let agent = state.agent(&workspace, sender, mcp);

    let events = async_stream::stream! {
        let _lock = lock;
        yield UiEvent::Workspace(workspace.snapshot());
        let runner = agent
            .prompt(request.message.clone())
            .conversation(session_id.clone())
            .max_turns(state.bundle.config.max_turns)
            // A no-op without the planning tools: the plan stays empty.
            .add_hook(PlanGuard::new(workspace.clone()));
        let runner = match &state.store.history {
            History::Postgres(store) => runner.memory(store.clone()),
            History::Memory(store) => runner.memory(store.clone()),
        };
        let mut run = runner.stream();
        loop {
            // Subagents report through the channel while the main run waits on
            // their `task` calls, so both sources are polled together.
            let next = tokio::select! {
                Some(event) = subagent_events.recv() => Err(event),
                item = run.next() => Ok(item),
            };
            let item = match next {
                Err(subagent_event) => { yield subagent_event; continue; }
                Ok(Some(item)) => item,
                Ok(None) => break,
            };
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(Item::Event(StreamEvent::Text { text, .. }))) => {
                    yield UiEvent::Text { delta: text };
                }
                Ok(MultiTurnStreamItem::ToolCall { tool_call }) => {
                    yield UiEvent::ToolCall {
                        id: tool_call.id.to_string(),
                        name: tool_call.function.name.to_string(),
                        args: serde_json::Value::Object(tool_call.function.arguments),
                    };
                }
                Ok(MultiTurnStreamItem::ToolResult { tool_result }) => {
                    yield UiEvent::ToolResult {
                        id: tool_result.call.to_string(),
                        name: tool_result.name.to_string(),
                        content: clip(&result_text(&tool_result.content), MAX_TOOL_RESULT_CHARS),
                        is_error: tool_result.is_error,
                    };
                    let snapshot = workspace.snapshot();
                    // A pod that dies mid-turn then loses at most the last tool's change.
                    if let Err(error) = state.store.save_workspace(&session_id, &snapshot).await {
                        tracing::warn!("could not save the workspace of {session_id}: {error:#}");
                    }
                    yield UiEvent::Workspace(snapshot);
                }
                Ok(MultiTurnStreamItem::ModelTurnRetried { .. }) => {
                    yield UiEvent::DiscardDraft;
                }
                Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                    let usage = response.usage();
                    yield UiEvent::Done {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        requests: response.requests(),
                    };
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::error!("agent run failed: {error}");
                    yield UiEvent::Error { message: error.to_string() };
                    break;
                }
            }
        }
        while let Ok(event) = subagent_events.try_recv() {
            yield event;
        }
    };

    let events = events.map(|event| Ok::<_, std::convert::Infallible>(Event::default().json_data(event).unwrap_or_default()));
    Sse::new(events).keep_alive(KeepAlive::default()).into_response()
}

async fn reset(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(request): Json<SessionRequest>) -> Response {
    let owner = bearer(&headers).and_then(subject);
    match state.store.reset(&request.session_id, owner.as_deref()).await {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => not_yours(),
        Err(error) => internal_error(error),
    }
}

fn not_yours() -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": "This chat belongs to someone else." }))).into_response()
}

/// Ready once the agent is loaded (before the server starts) and while the
/// session store answers. MCP servers are optional.
async fn ready(State(state): State<Arc<AppState>>) -> StatusCode {
    if state.store.ready().await { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE }
}

async fn info(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let bundle = &state.bundle;
    let documents: Vec<_> = state
        .knowledge
        .iter()
        .flat_map(|kb| &kb.documents)
        .map(|doc| json!({ "path": doc.path, "title": doc.title }))
        .collect();
    Json(json!({
        "name": bundle.name,
        "description": bundle.config.description,
        "examples": bundle.config.examples,
        "model": bundle.config.model,
        "mcp_tools": *state.mcp_tool_names.read().unwrap_or_else(std::sync::PoisonError::into_inner),
        "documents": documents,
        "skills": state.skills.iter().flat_map(|skills| skills.iter().map(|skill| &skill.name)).collect::<Vec<_>>(),
        "subagents": state.subagents.iter().map(|agent| &agent.name).collect::<Vec<_>>(),
        "store": state.store.kind(),
        "pod": std::env::var("POD_NAME").ok(),
    }))
}

fn result_text(content: &[ToolResultContent]) -> String {
    content
        .iter()
        .map(|part| match part {
            ToolResultContent::Text(text) => text.text.clone(),
            ToolResultContent::Json { value } => value.to_string(),
            ToolResultContent::Image(_) => "<image>".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

struct Args {
    dir: PathBuf,
    check: bool,
}

fn args() -> anyhow::Result<Args> {
    let mut dir = None;
    let mut check = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--check" => check = true,
            "-h" | "--help" => {
                println!("usage: agent [AGENT_DIR] [--check]\n\nAGENT_DIR defaults to $AGENT_DIR, then the working directory.");
                std::process::exit(0);
            }
            flag if flag.starts_with('-') => anyhow::bail!("unknown option {flag}; see --help"),
            path if dir.is_none() => dir = Some(PathBuf::from(path)),
            _ => anyhow::bail!("only one agent folder can be given"),
        }
    }
    let dir = dir.or_else(|| std::env::var_os("AGENT_DIR").map(PathBuf::from)).unwrap_or_else(|| ".".into());
    Ok(Args { dir, check })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Settings and secrets from a gitignored .env (searched upward from the working directory).
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        // No colour codes when the output is not a terminal, e.g. `kubectl logs`.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let args = args()?;
    let bundle = Bundle::load(&args.dir)?;
    if args.check {
        print_check(&bundle);
        return Ok(());
    }

    let addr = std::env::var("AGENT_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_owned());

    // As PID 1 in a container the process ignores SIGTERM until it has a
    // handler, so install one before the slow startup: a stop during startup
    // then exits at once instead of waiting out the whole grace period.
    let mut terminate = signal(SignalKind::terminate())?;
    let state = tokio::select! {
        state = start(bundle) => Arc::new(state?),
        _ = terminate.recv() => return Ok(()),
        _ = tokio::signal::ctrl_c() => return Ok(()),
    };

    // Sessions idle for longer than SESSION_TTL_DAYS (default 7; 0 keeps them) are deleted hourly.
    let ttl_days: f64 = std::env::var("SESSION_TTL_DAYS").ok().and_then(|days| days.parse().ok()).unwrap_or(7.0);
    if ttl_days > 0.0 {
        let state = state.clone();
        let idle = std::time::Duration::from_secs_f64(ttl_days * 86_400.0);
        tokio::spawn(async move {
            let mut hourly = tokio::time::interval(std::time::Duration::from_secs(3_600));
            loop {
                hourly.tick().await;
                match state.store.expire(idle).await {
                    Ok(0) => {}
                    Ok(gone) => tracing::info!("deleted {gone} conversations idle for over {ttl_days} days"),
                    Err(error) => tracing::warn!("session expiry failed: {error:#}"),
                }
            }
        });
    }

    let app = Router::new()
        // The shared UI, in single-agent mode (its default).
        .route("/", get(|| async { Html(include_str!("../../ui/index.html")) }))
        .route("/v1/info", get(info))
        .route("/v1/chat", post(chat))
        .route("/v1/reset", post(reset))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(ready))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("{} ({}) on http://{addr}", state.bundle.name, state.bundle.config.model);
    // Running agent turns are open requests, so they finish before the process exits.
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal(terminate)).await?;
    Ok(())
}

/// Connects the models, MCP servers and session store.
async fn start(mut bundle: Bundle) -> anyhow::Result<AppState> {
    let base_url = bundle.config.base_url.as_deref().filter(|url| !url.is_empty());
    let model = connect_model(&bundle.config.model, base_url)?;

    let mut subagents = Vec::new();
    if bundle.enabled(Builtin::Subagents) {
        let skills_section = match bundle.enabled(Builtin::Skills) {
            true => skills::prompt_section(&bundle.skills),
            false => String::new(),
        };
        for config in &bundle.config.subagents {
            subagents.push(Subagent {
                name: config.name.clone(),
                description: config.description.clone(),
                preamble: format!("{}{skills_section}", config.instructions.trim_end()),
                model: match &config.model {
                    Some(name) => connect_model(name, base_url)?,
                    None => model.clone(),
                },
                max_turns: config.max_turns,
            });
        }
        if subagents.is_empty() {
            subagents.push(Subagent {
                name: "general-purpose".to_owned(),
                description: "A worker with the same knowledge, skills and MCP tools as you.".to_owned(),
                preamble: format!("{GENERAL_PURPOSE}{skills_section}"),
                model: model.clone(),
                max_turns: 15,
            });
        }
    }

    // Without a user: a server that requires one (a gateway checking tokens)
    // gives no tools here, and its tools come with each signed-in user's turn.
    let shared_mcp_tools: Vec<DynamicTool> =
        futures::future::join_all(bundle.config.mcp_servers.iter().map(mcp_tools)).await.into_iter().flatten().collect();
    let mcp_tool_names = shared_mcp_tools.iter().map(|tool| tool.name().to_string()).collect();

    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let store = Store::open(env("DATABASE_URL"), env("DATABASE_SCHEMA")).await?;
    tracing::info!("sessions are kept in {}", store.kind());

    let knowledge = match bundle.enabled(Builtin::Knowledge) {
        true => bundle.knowledge.take().map(Arc::new),
        false => None,
    };
    let skills = bundle.enabled(Builtin::Skills).then(|| Arc::new(bundle.skills.clone()));
    Ok(AppState {
        system_prompt: bundle.system_prompt(),
        bundle,
        model,
        knowledge,
        skills,
        subagents: Arc::new(subagents),
        shared_mcp_tools,
        mcp_tool_names: std::sync::RwLock::new(mcp_tool_names),
        store,
    })
}

fn print_check(bundle: &Bundle) {
    let config = &bundle.config;
    let on = |builtin| if bundle.enabled(builtin) { "on" } else { "off" };
    println!("agent      {} ({})", bundle.name, bundle.dir.display());
    println!("model      {}{}", config.model, config.base_url.as_deref().map(|url| format!(" via {url}")).unwrap_or_default());
    println!("prompt     {} characters", bundle.system_prompt().len());
    println!(
        "tools      planning {} · files {} · knowledge {} · skills {} · subagents {}",
        on(Builtin::Planning),
        on(Builtin::Files),
        on(Builtin::Knowledge),
        on(Builtin::Skills),
        on(Builtin::Subagents)
    );
    if let Some(kb) = &bundle.knowledge {
        println!("knowledge  {}", kb.summary());
    }
    for skill in &bundle.skills {
        println!("skill      {}: {} ({} extra files)", skill.name, skill.description, skill.files.len());
    }
    for subagent in &config.subagents {
        println!("subagent   {} ({})", subagent.name, subagent.model.as_deref().unwrap_or(&config.model));
    }
    for server in &config.mcp_servers {
        let tools = server.tools.as_ref().map(|tools| format!(" [{}]", tools.join(", "))).unwrap_or_default();
        println!("mcp        {}{tools}", server.url);
    }
    println!("ok");
}

/// Resolves on Ctrl-C or SIGTERM (what Kubernetes and Docker send on stop).
async fn shutdown_signal(mut terminate: Signal) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    tracing::info!("shutting down; waiting for running requests to finish");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn reads_the_bearer_token_and_its_subject() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        // {"alg":"none"} . {"sub":"alice-id","groups":["sales"]} . (no signature)
        let token = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJhbGljZS1pZCIsImdyb3VwcyI6WyJzYWxlcyJdfQ.";
        headers.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {token}")).unwrap());
        assert_eq!(bearer(&headers), Some(token));
        assert_eq!(subject(token).as_deref(), Some("alice-id"));
        assert_eq!(subject("garbage"), None);
    }
}
