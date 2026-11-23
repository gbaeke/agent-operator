//! The reconcile loop. For each `Agent` the operator applies:
//!
//! - ConfigMap `<name>-agent`: the generated agent.yaml;
//! - Deployment `<name>`: agent-runtime with one projected volume at
//!   `/agent` in the folder layout the runtime reads (agent.yaml,
//!   skills/<name>/..., knowledge/...);
//! - Service `<name>`: port 3000.
//!
//! A hash of everything mounted goes into the pod template, so changing the
//! spec or a referenced ConfigMap rolls the pods. Every object is owned by
//! its Agent, so deleting the Agent deletes them. Objects are written with
//! server-side apply, so reconciling is idempotent.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMap, ConfigMapProjection, Container, ContainerPort, EnvVar, EnvVarSource, HTTPGetAction,
    KeyToPath, ObjectFieldSelector, PodSecurityContext, PodSpec, PodTemplateSpec, Probe, ProjectedVolumeSource,
    ResourceRequirements, SeccompProfile, SecurityContext, Service, ServicePort, ServiceSpec, Volume, VolumeMount,
    VolumeProjection,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{ObjectMeta, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;
use kube::{Api, Client, Resource, ResourceExt};
use serde_json::json;

use crate::crd::{Agent, AgentStatus};

const MANAGER: &str = "agent-operator";
const PORT: i32 = 3000;
const HASH_ANNOTATION: &str = "agents.baeke.info/config-hash";

/// Settings of the operator itself, from its environment.
pub struct Settings {
    /// The agent-runtime image for agents that name none (`AGENT_IMAGE`).
    pub image: String,
    /// `baseUrl` for agents that set none (`DEFAULT_BASE_URL`), e.g. a gateway.
    pub default_base_url: Option<String>,
    /// Environment for every agent pod, from `AGENT_ENV_<NAME>=value`: the
    /// cluster's values for `${NAME}` placeholders, like compose's
    /// `environment`. An agent's own `env` wins.
    pub agent_env: Vec<(String, String)>,
}

impl Settings {
    pub fn from_env() -> Self {
        Self {
            image: std::env::var("AGENT_IMAGE").unwrap_or_else(|_| "agent-runtime:dev".to_owned()),
            default_base_url: std::env::var("DEFAULT_BASE_URL").ok().filter(|url| !url.is_empty()),
            agent_env: std::env::vars()
                .filter_map(|(key, value)| Some((key.strip_prefix("AGENT_ENV_")?.to_owned(), value)))
                .filter(|(key, _)| !key.is_empty())
                .collect(),
        }
    }
}

struct Context {
    client: Client,
    settings: Settings,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Kube(#[from] kube::Error),
    #[error("rendering agent.yaml: {0}")]
    Render(#[from] serde_yaml_ng::Error),
}

/// Runs the controller until the process is stopped.
pub async fn run(client: Client, settings: Settings) -> anyhow::Result<()> {
    let agents = Api::<Agent>::all(client.clone());
    let config_maps = Api::<ConfigMap>::all(client.clone());
    let controller = Controller::new(agents, watcher::Config::default());
    let store = controller.store();
    let context = Arc::new(Context { client: client.clone(), settings });

    tracing::info!("watching Agents in all namespaces");
    controller
        .owns(Api::<Deployment>::all(client.clone()), watcher::Config::default())
        .owns(Api::<Service>::all(client.clone()), watcher::Config::default())
        .owns(config_maps.clone(), watcher::Config::default())
        // A skill or knowledge ConfigMap is not owned by the Agent; map its
        // changes to the Agents in its namespace that reference it.
        .watches(config_maps, watcher::Config::default(), move |config_map| {
            let namespace = config_map.namespace();
            let name = config_map.name_any();
            store
                .state()
                .into_iter()
                .filter(|agent| agent.namespace() == namespace && references(agent, &name))
                .map(|agent| ObjectRef::from_obj(&*agent))
                .collect::<Vec<_>>()
        })
        .shutdown_on_signal()
        .run(reconcile, error_policy, context)
        .for_each(|result| async move {
            match result {
                Ok((agent, _)) => tracing::debug!("reconciled {agent}"),
                // The Agent was deleted and its objects going away triggered one more run.
                Err(kube::runtime::controller::Error::ObjectNotFound(agent)) => tracing::debug!("{agent} is gone"),
                Err(error) => tracing::warn!("reconcile failed: {error}"),
            }
        })
        .await;
    Ok(())
}

fn references(agent: &Agent, config_map: &str) -> bool {
    agent.spec.skills.iter().any(|skill| skill.config_map == config_map)
        || agent.spec.knowledge.as_ref().is_some_and(|knowledge| knowledge.config_map == config_map)
}

fn error_policy(agent: Arc<Agent>, error: &Error, _: Arc<Context>) -> Action {
    tracing::warn!("agent {}/{}: {error}", agent.namespace().unwrap_or_default(), agent.name_any());
    Action::requeue(Duration::from_secs(30))
}

async fn reconcile(agent: Arc<Agent>, context: Arc<Context>) -> Result<Action, Error> {
    let namespace = agent.namespace().unwrap_or_else(|| "default".to_owned());
    let name = agent.name_any();
    let client = &context.client;
    let config_maps = Api::<ConfigMap>::namespaced(client.clone(), &namespace);
    let apply = PatchParams::apply(MANAGER).force();

    // The ConfigMaps the agent mounts; a missing one is reported, not fatal.
    let mut missing = Vec::new();
    let mut skills = Vec::new();
    for skill in &agent.spec.skills {
        match config_maps.get_opt(&skill.config_map).await? {
            Some(config_map) => skills.push((skill.folder().to_owned(), config_map)),
            None => missing.push(skill.config_map.clone()),
        }
    }
    let knowledge = match &agent.spec.knowledge {
        Some(reference) => {
            let found = config_maps.get_opt(&reference.config_map).await?;
            if found.is_none() {
                missing.push(reference.config_map.clone());
            }
            found
        }
        None => None,
    };

    let agent_yaml = render(&agent, &context.settings)?;
    let config_name = format!("{name}-agent");
    let config = ConfigMap {
        metadata: metadata(&agent, &config_name),
        data: Some(BTreeMap::from([("agent.yaml".to_owned(), agent_yaml.clone())])),
        ..Default::default()
    };
    config_maps.patch(&config_name, &apply, &Patch::Apply(&config)).await?;

    let hash = config_hash(&agent_yaml, &skills, knowledge.as_ref());
    let deployment = deployment(&agent, &context.settings, &config_name, &skills, knowledge.as_ref(), &hash);
    let deployments = Api::<Deployment>::namespaced(client.clone(), &namespace);
    let applied = deployments.patch(&name, &apply, &Patch::Apply(&deployment)).await?;
    Api::<Service>::namespaced(client.clone(), &namespace)
        .patch(&name, &apply, &Patch::Apply(&service(&agent)))
        .await?;

    // Ready once the rollout is done: the Deployment saw the latest spec, and
    // every pod runs it and is available, with no pods of an older version left.
    let wanted = agent.spec.replicas;
    let status = applied.status.clone().unwrap_or_default();
    let rolled_out = status.observed_generation >= applied.metadata.generation
        && status.updated_replicas.unwrap_or(0) == wanted
        && status.available_replicas.unwrap_or(0) == wanted
        && status.replicas.unwrap_or(0) == wanted;
    let ready_replicas = status.ready_replicas.unwrap_or(0);
    let ready = missing.is_empty() && rolled_out;
    let message = if !missing.is_empty() {
        Some(format!("missing ConfigMaps: {}", missing.join(", ")))
    } else if !ready {
        let updated = status.updated_replicas.unwrap_or(0);
        let old = (status.replicas.unwrap_or(0) - updated).max(0);
        let available = status.available_replicas.unwrap_or(0);
        Some(format!("rolling out: {updated}/{wanted} pods updated, {available} available, {old} old"))
    } else {
        None
    };
    let status = AgentStatus {
        ready,
        model: Some(resolve(&agent.spec.agent.model, &agent, &context.settings)),
        url: Some(format!("http://{name}.{namespace}:{PORT}")),
        ready_replicas,
        observed_generation: agent.metadata.generation,
        message,
    };
    if agent.status.as_ref() != Some(&status) {
        let patch = json!({ "apiVersion": Agent::api_version(&()), "kind": Agent::kind(&()), "status": status });
        Api::<Agent>::namespaced(client.clone(), &namespace)
            .patch_status(&name, &apply, &Patch::Apply(&patch))
            .await?;
        tracing::info!("agent {namespace}/{name}: {}", status.message.as_deref().unwrap_or("ready"));
    }
    // Deployment changes requeue the agent anyway; this is a safety net.
    Ok(Action::requeue(Duration::from_secs(if missing.is_empty() { 300 } else { 30 })))
}

/// `${VAR}` and `${VAR:-default}` in `text`, resolved from the environment
/// the agent's pods get: the operator's `AGENT_ENV_*` and the agent's own
/// literal `env` values. A placeholder that cannot be resolved stays as is.
fn resolve(text: &str, agent: &Agent, settings: &Settings) -> String {
    let lookup = |name: &str| {
        let own = agent.spec.env.iter().rev().find(|var| var.name == name).and_then(|var| var.value.clone());
        own.or_else(|| settings.agent_env.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone()))
            .filter(|value| !value.is_empty())
    };
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let Some(end) = rest[start..].find('}').map(|end| start + end) else { break };
        out.push_str(&rest[..start]);
        let inner = &rest[start + 2..end];
        let (name, default) = inner.split_once(":-").map_or((inner, None), |(name, default)| (name, Some(default)));
        match lookup(name).or(default.map(str::to_owned)) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// agent.yaml for the pod: the spec's agent fields, with the resource name
/// as the default display name and the operator's default base URL.
pub fn render(agent: &Agent, settings: &Settings) -> Result<String, serde_yaml_ng::Error> {
    let mut config = agent.spec.agent.clone();
    config.name.get_or_insert_with(|| agent.name_any());
    if config.base_url.is_none() {
        config.base_url.clone_from(&settings.default_base_url);
    }
    serde_yaml_ng::to_string(&config)
}

fn labels(agent: &Agent) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".to_owned(), "agent".to_owned()),
        ("app.kubernetes.io/instance".to_owned(), agent.name_any()),
        ("app.kubernetes.io/managed-by".to_owned(), MANAGER.to_owned()),
    ])
}

fn metadata(agent: &Agent, name: &str) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_owned()),
        namespace: agent.namespace(),
        labels: Some(labels(agent)),
        owner_references: agent.controller_owner_ref(&()).map(|owner| vec![owner]),
        ..Default::default()
    }
}

/// A stable hash (FNV-1a) of everything the pod mounts.
fn config_hash(agent_yaml: &str, skills: &[(String, ConfigMap)], knowledge: Option<&ConfigMap>) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |text: &str| {
        for byte in text.bytes().chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(agent_yaml);
    let sources = skills.iter().map(|(folder, config_map)| (folder.as_str(), config_map));
    for (prefix, config_map) in sources.chain(knowledge.map(|config_map| ("knowledge", config_map))) {
        feed(prefix);
        for (key, value) in config_map.data.iter().flatten() {
            feed(key);
            feed(value);
        }
    }
    format!("{hash:016x}")
}

/// One projection of a ConfigMap, every key placed under `prefix/`.
fn projection(config_map: &ConfigMap, prefix: &str) -> VolumeProjection {
    let items = config_map
        .data
        .iter()
        .flatten()
        .map(|(key, _)| KeyToPath { key: key.clone(), path: format!("{prefix}/{key}"), mode: None })
        .collect();
    VolumeProjection {
        config_map: Some(ConfigMapProjection {
            name: config_map.metadata.name.clone().unwrap_or_default(),
            items: Some(items),
            optional: Some(false),
        }),
        ..Default::default()
    }
}

fn deployment(
    agent: &Agent,
    settings: &Settings,
    config_name: &str,
    skills: &[(String, ConfigMap)],
    knowledge: Option<&ConfigMap>,
    hash: &str,
) -> Deployment {
    let name = agent.name_any();
    let mut sources = vec![VolumeProjection {
        config_map: Some(ConfigMapProjection {
            name: config_name.to_owned(),
            items: Some(vec![KeyToPath { key: "agent.yaml".to_owned(), path: "agent.yaml".to_owned(), mode: None }]),
            optional: Some(false),
        }),
        ..Default::default()
    }];
    sources.extend(skills.iter().map(|(folder, config_map)| projection(config_map, &format!("skills/{folder}"))));
    sources.extend(knowledge.map(|config_map| projection(config_map, "knowledge")));

    let mut env = vec![EnvVar {
        name: "POD_NAME".to_owned(),
        value_from: Some(EnvVarSource {
            field_ref: Some(ObjectFieldSelector { field_path: "metadata.name".to_owned(), ..Default::default() }),
            ..Default::default()
        }),
        ..Default::default()
    }];
    // Later entries win in Kubernetes, so the agent's own env overrides the defaults.
    env.extend(settings.agent_env.iter().map(|(name, value)| EnvVar {
        name: name.clone(),
        value: Some(value.clone()),
        ..Default::default()
    }));
    env.extend(agent.spec.env.iter().cloned());

    let probe = |path: &str, period: i32| Probe {
        http_get: Some(HTTPGetAction { path: Some(path.to_owned()), port: IntOrString::Int(PORT), ..Default::default() }),
        period_seconds: Some(period),
        ..Default::default()
    };
    let resources = agent.spec.resources.clone().unwrap_or_else(|| ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_owned(), Quantity("50m".to_owned())),
            ("memory".to_owned(), Quantity("64Mi".to_owned())),
        ])),
        limits: Some(BTreeMap::from([("memory".to_owned(), Quantity("256Mi".to_owned()))])),
        ..Default::default()
    });

    let container = Container {
        name: "agent".to_owned(),
        image: Some(agent.spec.image.clone().unwrap_or_else(|| settings.image.clone())),
        image_pull_policy: Some("IfNotPresent".to_owned()),
        ports: Some(vec![ContainerPort { name: Some("http".to_owned()), container_port: PORT, ..Default::default() }]),
        env: Some(env),
        volume_mounts: Some(vec![VolumeMount {
            name: "agent".to_owned(),
            mount_path: "/agent".to_owned(),
            read_only: Some(true),
            ..Default::default()
        }]),
        readiness_probe: Some(probe("/readyz", 5)),
        liveness_probe: Some(probe("/healthz", 20)),
        resources: Some(resources),
        security_context: Some(SecurityContext {
            allow_privilege_escalation: Some(false),
            read_only_root_filesystem: Some(true),
            run_as_non_root: Some(true),
            capabilities: Some(Capabilities { drop: Some(vec!["ALL".to_owned()]), ..Default::default() }),
            ..Default::default()
        }),
        ..Default::default()
    };

    let labels = labels(agent);
    Deployment {
        metadata: metadata(agent, &name),
        spec: Some(DeploymentSpec {
            replicas: Some(agent.spec.replicas),
            selector: LabelSelector {
                match_labels: Some(BTreeMap::from([("app.kubernetes.io/instance".to_owned(), name.clone())])),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    annotations: Some(BTreeMap::from([(HASH_ANNOTATION.to_owned(), hash.to_owned())])),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![container],
                    volumes: Some(vec![Volume {
                        name: "agent".to_owned(),
                        projected: Some(ProjectedVolumeSource { sources: Some(sources), ..Default::default() }),
                        ..Default::default()
                    }]),
                    security_context: Some(PodSecurityContext {
                        run_as_non_root: Some(true),
                        seccomp_profile: Some(SeccompProfile { type_: "RuntimeDefault".to_owned(), ..Default::default() }),
                        ..Default::default()
                    }),
                    // Agent turns are open requests; give them time to finish on a rollout.
                    termination_grace_period_seconds: Some(120),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn service(agent: &Agent) -> Service {
    let name = agent.name_any();
    Service {
        metadata: metadata(agent, &name),
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([("app.kubernetes.io/instance".to_owned(), name)])),
            ports: Some(vec![ServicePort {
                name: Some("http".to_owned()),
                port: PORT,
                target_port: Some(IntOrString::Int(PORT)),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_map(name: &str, data: &[(&str, &str)]) -> ConfigMap {
        ConfigMap {
            metadata: ObjectMeta { name: Some(name.to_owned()), ..Default::default() },
            data: Some(data.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()),
            ..Default::default()
        }
    }

    fn agent() -> Agent {
        Agent::new("quokka", serde_json::from_value(json!({ "replicas": 2 })).unwrap())
    }

    #[test]
    fn mounts_the_agent_folder_layout() {
        let settings = Settings {
            image: "agent-runtime:dev".into(),
            default_base_url: None,
            agent_env: vec![("MCP_URL".into(), "http://mcp-server.agents:8081/mcp".into())],
        };
        let skills = vec![("sla-credit".to_owned(), config_map("skill-sla-credit", &[("SKILL.md", "x"), ("example.md", "y")]))];
        let knowledge = config_map("quokka-kb", &[("pricing.md", "z")]);
        let deployment = deployment(&agent(), &settings, "quokka-agent", &skills, Some(&knowledge), "hash");

        let pod = deployment.spec.as_ref().unwrap().template.spec.as_ref().unwrap();
        let volume = pod.volumes.as_ref().unwrap()[0].projected.as_ref().unwrap();
        let paths: Vec<String> = volume
            .sources
            .iter()
            .flatten()
            .flat_map(|source| source.config_map.as_ref().unwrap().items.clone().unwrap())
            .map(|item| item.path)
            .collect();
        assert_eq!(
            paths,
            ["agent.yaml", "skills/sla-credit/SKILL.md", "skills/sla-credit/example.md", "knowledge/pricing.md"]
        );
        assert_eq!(pod.containers[0].image.as_deref(), Some("agent-runtime:dev"));
        let env: Vec<&str> = pod.containers[0].env.iter().flatten().map(|var| var.name.as_str()).collect();
        assert_eq!(env, ["POD_NAME", "MCP_URL"]);
        assert_eq!(deployment.spec.as_ref().unwrap().replicas, Some(2));
    }

    #[test]
    fn resolves_placeholders_like_the_pod() {
        let settings = Settings {
            image: String::new(),
            default_base_url: None,
            agent_env: vec![("AGENT_MODEL".into(), "openai/azure-gpt-6.1-sol".into())],
        };
        let agent = agent();
        assert_eq!(resolve("${AGENT_MODEL:-openai/x}", &agent, &settings), "openai/azure-gpt-6.1-sol");
        assert_eq!(resolve("${OTHER:-openai/x}", &agent, &settings), "openai/x");
        assert_eq!(resolve("${UNSET}", &agent, &settings), "${UNSET}");
        assert_eq!(resolve("anthropic/claude", &agent, &settings), "anthropic/claude");
    }

    #[test]
    fn hash_changes_with_any_mounted_content() {
        let skill = |text: &str| vec![("s".to_owned(), config_map("skill-s", &[("SKILL.md", text)]))];
        let base = config_hash("model: x", &skill("a"), None);
        assert_eq!(base, config_hash("model: x", &skill("a"), None));
        assert_ne!(base, config_hash("model: y", &skill("a"), None));
        assert_ne!(base, config_hash("model: x", &skill("b"), None));
        assert_ne!(base, config_hash("model: x", &skill("a"), Some(&config_map("kb", &[("a.md", "")]))));
    }
}
