//! A small MCP server with general-purpose tools, served over streamable HTTP.
//!
//! Tools: `calculate`, `current_time`, `get_weather` (Open-Meteo, no API key)
//! and `fetch_url`, plus `whoami` and `my_tickets`, which act as the signed-in
//! user (see `auth.rs`). Run with `cargo run -p mcp-server`; it listens on
//! `MCP_ADDR` (default `127.0.0.1:8081`) at the path `/mcp`.

mod auth;

use std::time::Duration;

use auth::Caller;
use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;

use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use serde::Deserialize;
use tokio::signal::unix::{Signal, SignalKind, signal};
use serde_json::{Value, json};

/// Longest page text `fetch_url` returns, in characters.
const MAX_PAGE_CHARS: usize = 12_000;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CalculateArgs {
    /// Arithmetic expression, e.g. `(1250 * 12) * 0.85` or `math::sqrt(2)`.
    pub expression: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TimeArgs {
    /// IANA time zone such as `Europe/Brussels`. Defaults to UTC.
    pub timezone: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WeatherArgs {
    /// City name, optionally with country, e.g. `Ghent` or `Paris, France`.
    pub city: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FetchArgs {
    /// Absolute http(s) URL to fetch.
    pub url: String,
}

/// Rewrites integer literals as floats (`7/2` becomes `7.0/2.0`), because
/// evalexpr divides integers like Rust does and `252/43200` would be 0.
fn as_floats(expression: &str) -> String {
    let chars: Vec<char> = expression.chars().collect();
    let mut out = String::with_capacity(expression.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let starts_number = chars[i].is_ascii_digit()
            && !(i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_' || chars[i - 1] == '.'));
        if !starts_number {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
            i += 1;
        }
        out.extend(chars[start..i].iter().filter(|c| **c != '_'));
        let is_float = i < chars.len() && matches!(chars[i], '.' | 'e' | 'E');
        if !is_float {
            out.push_str(".0");
        }
    }
    out
}

#[derive(Clone)]
pub struct Toolbox {
    http: reqwest::Client,
    tool_router: ToolRouter<Toolbox>,
}

fn text(value: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.into())])
}

fn failure(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

#[tool_router]
impl Toolbox {
    fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("rust-agents-mcp/0.1")
            .build()
            .expect("the HTTP client builds");
        Self {
            http,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Evaluate an arithmetic expression exactly. Supports + - * / % ^, \
                       parentheses and functions like math::sqrt, math::ln, round, floor, min, max. \
                       Use this instead of doing arithmetic in your head."
    )]
    fn calculate(&self, Parameters(args): Parameters<CalculateArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(match evalexpr::eval(&as_floats(&args.expression)) {
            Ok(value) => text(format!("{} = {value}", args.expression)),
            Err(error) => failure(format!("could not evaluate `{}`: {error}", args.expression)),
        })
    }

    #[tool(description = "Current date, time and weekday in an IANA time zone (default UTC).")]
    fn current_time(&self, Parameters(args): Parameters<TimeArgs>) -> Result<CallToolResult, ErrorData> {
        let name = args.timezone.unwrap_or_else(|| "UTC".to_owned());
        let Ok(zone) = name.parse::<chrono_tz::Tz>() else {
            return Ok(failure(format!("unknown time zone `{name}`; use an IANA name like Europe/Brussels")));
        };
        let now = chrono::Utc::now().with_timezone(&zone);
        Ok(text(format!("{} ({name})", now.format("%A %Y-%m-%d %H:%M:%S %Z"))))
    }

    #[tool(description = "Current weather and a 3-day forecast for a city, from Open-Meteo.")]
    async fn get_weather(&self, Parameters(args): Parameters<WeatherArgs>) -> Result<CallToolResult, ErrorData> {
        Ok(match self.weather(&args.city).await {
            Ok(report) => text(report.to_string()),
            Err(error) => failure(format!("weather lookup failed: {error}")),
        })
    }

    #[tool(description = "Fetch a web page and return its readable text (truncated to about 12k characters).")]
    async fn fetch_url(&self, Parameters(args): Parameters<FetchArgs>) -> Result<CallToolResult, ErrorData> {
        if !(args.url.starts_with("http://") || args.url.starts_with("https://")) {
            return Ok(failure("only http and https URLs are supported"));
        }
        Ok(match self.page_text(&args.url).await {
            Ok(page) => text(page),
            Err(error) => failure(format!("fetching {} failed: {error}", args.url)),
        })
    }

    #[tool(
        description = "Who the signed-in user is (username, name, email, groups), from the token this tool \
                       call carried. Use it when the user asks who they are, or before acting on their behalf."
    )]
    async fn whoami(&self, Extension(parts): Extension<Parts>) -> Result<CallToolResult, ErrorData> {
        Ok(match parts.extensions.get::<Caller>() {
            Some(caller) => text(serde_json::to_string_pretty(caller).unwrap_or_default()),
            None => text("No signed-in user: this call carried no user token (the server runs without sign-in)."),
        })
    }

    #[tool(
        description = "The signed-in user's open tickets in the ticket system. Every user sees only their own. \
                       (Demo data, made up per user and group.)"
    )]
    async fn my_tickets(&self, Extension(parts): Extension<Parts>) -> Result<CallToolResult, ErrorData> {
        Ok(match parts.extensions.get::<Caller>() {
            Some(caller) => text(serde_json::to_string_pretty(&tickets_for(caller)).unwrap_or_default()),
            None => failure("my_tickets needs a signed-in user, and this call carried no user token."),
        })
    }
}

/// Made-up tickets for a user: by group, numbered from their username, so
/// each user gets their own stable list, as a downstream system would return.
fn tickets_for(caller: &Caller) -> Vec<Value> {
    let by_group: &[(&str, &[(&str, &str)])] = &[
        ("sales", &[
            ("Quote for 200 Fleet Pro trackers for Contoso", "waiting on customer"),
            ("Fabrikam asks for a renewal discount", "open"),
        ]),
        ("engineering", &[
            ("Firmware 4.2 rollout stuck on 3% of devices", "in progress"),
            ("Flaky integration test in fleet-api", "open"),
        ]),
        ("platform-admins", &[
            ("Rotate the Keycloak admin password", "open"),
            ("Postgres volume at 78%", "in progress"),
        ]),
    ];
    let mut titles: Vec<(&str, &str)> = by_group
        .iter()
        .filter(|(group, _)| caller.groups.iter().any(|g| g == group))
        .flat_map(|(_, tickets)| tickets.iter().copied())
        .collect();
    titles.push(("Laptop battery swells, replacement requested", "open"));
    let base = caller.username.bytes().fold(1000u32, |n, b| (n * 31 + u32::from(b)) % 9000) + 1000;
    titles
        .into_iter()
        .enumerate()
        .map(|(i, (title, status))| {
            json!({ "id": format!("TCK-{}", base + i as u32), "title": title, "status": status, "assignee": caller.username })
        })
        .collect()
}

impl Toolbox {
    async fn weather(&self, city: &str) -> anyhow::Result<Value> {
        // Open-Meteo's geocoder matches a bare place name, so drop any ", country" suffix.
        let place = city.split(',').next().unwrap_or(city).trim();
        let geo: Value = self
            .http
            .get("https://geocoding-api.open-meteo.com/v1/search")
            .query(&[("name", place), ("count", "1")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let hit = geo["results"]
            .get(0)
            .ok_or_else(|| anyhow::anyhow!("no place called `{place}` found"))?;
        let (lat, lon) = (hit["latitude"].to_string(), hit["longitude"].to_string());

        let forecast: Value = self
            .http
            .get("https://api.open-meteo.com/v1/forecast")
            .query(&[
                ("latitude", lat.as_str()),
                ("longitude", lon.as_str()),
                ("current", "temperature_2m,apparent_temperature,relative_humidity_2m,wind_speed_10m,weather_code"),
                ("daily", "temperature_2m_max,temperature_2m_min,precipitation_probability_max,weather_code"),
                ("forecast_days", "3"),
                ("timezone", "auto"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(json!({
            "place": format!("{}, {}", hit["name"].as_str().unwrap_or(place), hit["country"].as_str().unwrap_or("")),
            "current": forecast["current"],
            "current_units": forecast["current_units"],
            "daily": forecast["daily"],
            "weather_code_note": "WMO codes: 0 clear, 1-3 partly cloudy, 45/48 fog, 51-67 drizzle/rain, 71-77 snow, 80-82 showers, 95-99 thunderstorm",
        }))
    }

    async fn page_text(&self, url: &str) -> anyhow::Result<String> {
        let response = self.http.get(url).send().await?.error_for_status()?;
        let is_html = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("html"));
        let body = response.bytes().await?;
        let page = if is_html {
            html2text::from_read(&body[..], 100)?
        } else {
            String::from_utf8_lossy(&body).into_owned()
        };
        Ok(match page.char_indices().nth(MAX_PAGE_CHARS) {
            Some((cut, _)) => format!("{}\n\n[truncated]", &page[..cut]),
            None => page,
        })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Toolbox {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::new("quokka-tools", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "General-purpose tools: exact math, time zones, live weather and web page fetching; \
                 and, as the signed-in user, who they are and their tickets.",
            )
    }
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

    // Installed first: as PID 1 in a container the process ignores SIGTERM without a handler.
    let terminate = signal(SignalKind::terminate())?;
    let addr = std::env::var("MCP_ADDR").unwrap_or_else(|_| "127.0.0.1:8081".to_owned());
    // Hostnames clients may use to reach us (DNS-rebinding protection), e.g.
    // `mcp-server` inside docker compose. Loopback only by default.
    let allowed_hosts = std::env::var("MCP_ALLOWED_HOSTS").unwrap_or_else(|_| "localhost,127.0.0.1,::1".to_owned());
    // Stateless: no MCP session is kept between requests, so any replica can
    // serve any request and a restart loses nothing.
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_hosts(allowed_hosts.split(',').map(str::trim).filter(|host| !host.is_empty()));
    let service = StreamableHttpService::new(|| Ok(Toolbox::new()), LocalSessionManager::default().into(), config);
    // Every MCP request needs a valid user token when OIDC_ISSUER is set.
    let verifier = auth::Verifier::from_env();
    match &verifier {
        Some(verifier) => tracing::info!("user tokens required; issuer {}", verifier.issuer()),
        None => tracing::warn!("no OIDC_ISSUER: tools run without a user"),
    }
    let mcp = axum::Router::new()
        .fallback_service(service)
        .layer(axum::middleware::from_fn_with_state(verifier, auth::require_caller));
    let router = axum::Router::new()
        .nest_service("/mcp", mcp)
        .route("/healthz", axum::routing::get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("MCP server listening on http://{addr}/mcp");
    axum::serve(listener, router).with_graceful_shutdown(shutdown_signal(terminate)).await?;
    Ok(())
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

    #[test]
    fn tickets_follow_the_user() {
        let caller = |name: &str, groups: &[&str]| Caller {
            sub: name.into(),
            username: name.into(),
            name: None,
            email: None,
            groups: groups.iter().map(|g| g.to_string()).collect(),
            expires_at: 0,
            client: None,
        };
        let alice = tickets_for(&caller("alice", &["sales"]));
        let bob = tickets_for(&caller("bob", &["engineering"]));
        assert_eq!(alice.len(), 3);
        assert!(alice[0]["title"].as_str().unwrap().contains("Contoso"));
        assert!(bob[0]["title"].as_str().unwrap().contains("Firmware"));
        assert_ne!(alice[0]["id"], bob[0]["id"]);
        assert_eq!(alice, tickets_for(&caller("alice", &["sales"])));
    }

    #[test]
    fn integer_division_is_exact() {
        let value = evalexpr::eval(&as_floats("(43200-252)/43200*100")).unwrap();
        assert_eq!(value.as_float().unwrap(), (43200.0 - 252.0) / 43200.0 * 100.0);
    }

    #[test]
    fn leaves_floats_and_names_alone() {
        assert_eq!(as_floats("1.5 * 2 + math::sqrt(16) + x2"), "1.5 * 2.0 + math::sqrt(16.0) + x2");
        assert_eq!(as_floats("1e3 + 140_000"), "1e3 + 140000.0");
    }
}
