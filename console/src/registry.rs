//! Where the console finds agents.
//!
//! - Static: `CONSOLE_AGENTS=id=url,...`, for compose and local runs.
//! - Kubernetes: `CONSOLE_DISCOVERY=kubernetes` watches the `Agent`
//!   resources in `CONSOLE_NAMESPACE` (default `agents`) and uses each
//!   one's `status.url`. A new, changed or deleted Agent is announced on
//!   `changes`, so the UI can update at once.

use anyhow::{Context, bail};
use futures::StreamExt;
use kube::api::{ApiResource, DynamicObject, GroupVersionKind};
use kube::runtime::{WatchStreamExt, reflector, watcher};
use kube::{Api, ResourceExt};
use tokio::sync::broadcast;

#[derive(Clone, Debug, PartialEq)]
pub struct AgentRef {
    pub id: String,
    pub url: String,
    /// Why the agent cannot be used yet, e.g. its rollout is not finished.
    pub not_ready: Option<String>,
}

pub enum Registry {
    Static(Vec<AgentRef>),
    Kubernetes(reflector::Store<DynamicObject>),
}

impl Registry {
    /// The registry the environment asks for, and a channel that fires when
    /// the list of agents changes (never, for a static list).
    pub async fn from_env() -> anyhow::Result<(Self, broadcast::Sender<()>)> {
        let (changes, _) = broadcast::channel(16);
        match std::env::var("CONSOLE_DISCOVERY").as_deref() {
            Ok("kubernetes") => {
                let namespace = std::env::var("CONSOLE_NAMESPACE").unwrap_or_else(|_| "agents".to_owned());
                let store = watch_agents(&namespace, changes.clone()).await?;
                tracing::info!("watching Agents in namespace {namespace}");
                Ok((Self::Kubernetes(store), changes))
            }
            Ok("static") | Err(_) => {
                let spec = std::env::var("CONSOLE_AGENTS").unwrap_or_default();
                Ok((Self::Static(parse_agents(&spec)?), changes))
            }
            Ok(other) => bail!("CONSOLE_DISCOVERY must be `static` or `kubernetes`, not `{other}`"),
        }
    }

    pub fn agents(&self) -> Vec<AgentRef> {
        match self {
            Self::Static(agents) => agents.clone(),
            Self::Kubernetes(store) => {
                let mut agents: Vec<AgentRef> = store.state().iter().filter_map(|agent| from_resource(agent)).collect();
                agents.sort_by(|a, b| a.id.cmp(&b.id));
                agents
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<AgentRef> {
        self.agents().into_iter().find(|agent| agent.id == id)
    }
}

/// An `Agent` resource as the console sees it: its name and status.
fn from_resource(agent: &DynamicObject) -> Option<AgentRef> {
    let status = agent.data.get("status");
    let field = |name: &str| status.and_then(|status| status.get(name));
    let url = field("url").and_then(|url| url.as_str())?.trim_end_matches('/').to_owned();
    let ready = field("ready").and_then(|ready| ready.as_bool()).unwrap_or(false);
    let message = field("message").and_then(|message| message.as_str()).unwrap_or("starting");
    Some(AgentRef { id: agent.name_any(), url, not_ready: (!ready).then(|| message.to_owned()) })
}

async fn watch_agents(namespace: &str, changes: broadcast::Sender<()>) -> anyhow::Result<reflector::Store<DynamicObject>> {
    let client = kube::Client::try_default().await.context("no Kubernetes configuration for CONSOLE_DISCOVERY=kubernetes")?;
    let resource = ApiResource::from_gvk(&GroupVersionKind::gvk("agents.baeke.info", "v1alpha1", "Agent"));
    let api = Api::<DynamicObject>::namespaced_with(client, namespace, &resource);
    let writer = reflector::store::Writer::new(resource);
    let store = writer.as_reader();
    let events = watcher(api, watcher::Config::default()).default_backoff().reflect(writer);
    tokio::spawn(async move {
        futures::pin_mut!(events);
        while let Some(event) = events.next().await {
            match event {
                // The initial listing sends one event per Agent; announce it once, when it is done.
                Ok(watcher::Event::Init | watcher::Event::InitApply(_)) => {}
                Ok(_) => {
                    let _ = changes.send(());
                }
                Err(error) => tracing::warn!("watching Agents: {error}"),
            }
        }
    });
    store.wait_until_ready().await.context("listing Agents")?;
    Ok(store)
}

pub fn parse_agents(spec: &str) -> anyhow::Result<Vec<AgentRef>> {
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
        agents.push(AgentRef { id: id.to_owned(), url: url.to_owned(), not_ready: None });
    }
    Ok(agents)
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

    fn resource(status: serde_json::Value) -> DynamicObject {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "agents.baeke.info/v1alpha1", "kind": "Agent",
            "metadata": { "name": "pirate", "namespace": "agents" },
            "spec": {}, "status": status,
        }))
        .unwrap()
    }

    #[test]
    fn reads_agent_resources() {
        let ready = from_resource(&resource(serde_json::json!({ "ready": true, "url": "http://pirate.agents:3000" })));
        assert_eq!(
            ready,
            Some(AgentRef { id: "pirate".into(), url: "http://pirate.agents:3000".into(), not_ready: None })
        );
        let rolling = from_resource(&resource(serde_json::json!({ "ready": false, "url": "http://p:3000", "message": "rolling out" })));
        assert_eq!(rolling.unwrap().not_ready.as_deref(), Some("rolling out"));
        // Not reconciled yet: no URL, so not listed.
        assert_eq!(from_resource(&resource(serde_json::json!({}))), None);
    }
}
