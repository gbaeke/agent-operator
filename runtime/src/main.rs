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
use axum::http::StatusCode;
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
    mcp_tools: Vec<DynamicTool>,
    store: Store,
}

impl AppState {
    /// The main agent for one run, its tools bound to the session's workspace.
    fn agent(&self, workspace: &SharedWorkspace, events: EventSender) -> Agent {
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
                    mcp: self.mcp_tools.clone(),
                },
                events,
            });
        }
        builder.dynamic_tools(self.mcp_tools.clone()).build()
    }
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

async fn connect_mcp(url: &str) -> anyhow::Result<Vec<DynamicTool>> {
    // The newest protocol version with `initialize`: rmcp offers a newer one
    // by default, which agentgateway refuses on an `initialize` request.
    let config = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new("agent-runtime", env!("CARGO_PKG_VERSION")),
    )
    .with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE);
    let transport = StreamableHttpClientTransport::from_uri(url);
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
        let connect = tokio::time::timeout(std::time::Duration::from_secs(2), connect_mcp(url));
        match connect.await.unwrap_or_else(|_| Err(anyhow::anyhow!("timed out"))) {
            Ok(mut tools) => {
                if let Some(allowed) = &server.tools {
                    tools.retain(|tool| allowed.iter().any(|name| name == tool.name()));
                }
                tracing::info!("MCP server {url}: {} tools", tools.len());
                return tools;
            }
            Err(error) if attempt == 10 => tracing::warn!("no tools from MCP server {url}: {error}"),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
    Vec::new()
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
async fn chat(State(state): State<Arc<AppState>>, Json(request): Json<ChatRequest>) -> Response {
    let session_id = request.session_id.clone();
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
    if let Err(error) = state.store.touch(&session_id).await {
        return internal_error(error);
    }
    let workspace = match state.store.load_workspace(&session_id).await {
        Ok(workspace) => SharedWorkspace::new(workspace),
        Err(error) => return internal_error(error),
    };
    let (sender, mut subagent_events) = mpsc::unbounded_channel();
    let agent = state.agent(&workspace, sender);

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

async fn reset(State(state): State<Arc<AppState>>, Json(request): Json<SessionRequest>) -> Response {
    match state.store.reset(&request.session_id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(error) => internal_error(error),
    }
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
        "mcp_tools": state.mcp_tools.iter().map(|tool| tool.name().to_string()).collect::<Vec<_>>(),
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

    let mcp_tools: Vec<DynamicTool> =
        futures::future::join_all(bundle.config.mcp_servers.iter().map(mcp_tools)).await.into_iter().flatten().collect();

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
        mcp_tools,
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
