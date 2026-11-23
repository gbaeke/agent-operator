//! agent-studio: a local web app to define an agent in a form and run it
//! in the cluster. It talks to the Kubernetes API with your kubeconfig and
//! only listens on 127.0.0.1.
//!
//! ```text
//! cargo run -p agent-studio        # http://127.0.0.1:8090
//! ```
//!
//! For an agent it writes, with server-side apply (field manager
//! `agent-studio`), one ConfigMap per skill written in the studio
//! (`<agent>-skill-<skill>`), a ConfigMap `<agent>-knowledge` for uploaded
//! documents, and the `Agent`. The operator does the rest.
//!
//! Settings: `STUDIO_CONTEXT` (kubeconfig context, default `kind-agents`),
//! `STUDIO_NAMESPACE` (default `agents`), `STUDIO_ADDR` (default
//! `127.0.0.1:8090`), `CONSOLE_URL` (default `http://localhost:32000`).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ListParams, ObjectMeta, Patch, PatchParams};
use kube::config::KubeConfigOptions;
use kube::{Api, Client, ResourceExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MANAGER: &str = "agent-studio";
const PART_OF: &str = "app.kubernetes.io/part-of";
const MANAGED_BY: &str = "app.kubernetes.io/managed-by";

struct AppState {
    client: Client,
    namespace: String,
    context: String,
    console: String,
}

/// What the form sends. `spec` holds the agent.yaml fields; the studio adds
/// the Kubernetes ones from `skills`, `existingSkills` and `knowledge`.
#[derive(Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct Draft {
    name: String,
    #[serde(default)]
    spec: serde_json::Map<String, Value>,
    /// Skills written in the studio, each becoming a ConfigMap.
    #[serde(default)]
    skills: Vec<InlineSkill>,
    /// Skill ConfigMaps that already exist in the namespace.
    #[serde(default)]
    existing_skills: Vec<String>,
    /// Uploaded markdown documents, by file name.
    #[serde(default)]
    knowledge: BTreeMap<String, String>,
    /// A knowledge ConfigMap the studio did not make (kept as is).
    #[serde(default)]
    knowledge_ref: Option<String>,
}

#[derive(Deserialize, Serialize, Default, Clone)]
struct InlineSkill {
    name: String,
    description: String,
    instructions: String,
}

fn agent_resource() -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk("agents.baeke.info", "v1alpha1", "Agent"))
}

fn error(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({ "error": message.to_string() }))).into_response()
}

/// The API server's own words, e.g. `spec.tools[0]: Unsupported value: "shell"`.
fn kube_error(failure: kube::Error) -> Response {
    match failure {
        kube::Error::Api(status) => error(StatusCode::BAD_REQUEST, status.message),
        other => error(StatusCode::BAD_GATEWAY, format!("Kubernetes API: {other}")),
    }
}

/// A Kubernetes name usable for the Agent, its Deployment and its Service.
fn check_name(name: &str, what: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 50
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok { Ok(()) } else { Err(format!("{what} `{name}`: use lowercase letters, digits and -, at most 50 characters")) }
}

fn skill_md(skill: &InlineSkill) -> String {
    let description = skill.description.replace('\n', " ");
    format!(
        "---\nname: {}\ndescription: {}\n---\n\n{}\n",
        skill.name,
        serde_json::to_string(description.trim()).unwrap_or_default(),
        skill.instructions.trim()
    )
}

/// Reads a SKILL.md back into the form's fields.
fn parse_skill(name: &str, text: &str) -> InlineSkill {
    let mut skill = InlineSkill { name: name.to_owned(), ..Default::default() };
    let rest = text.trim_start().strip_prefix("---");
    if let Some((front, body)) = rest.and_then(|rest| rest.split_once("\n---")) {
        if let Ok(meta) = serde_yaml_ng::from_str::<BTreeMap<String, Value>>(front)
            && let Some(Value::String(description)) = meta.get("description")
        {
            skill.description = description.clone();
        }
        skill.instructions = body.split_once('\n').map_or("", |(_, body)| body).trim().to_owned();
    } else {
        skill.instructions = text.trim().to_owned();
    }
    skill
}

/// The objects a draft becomes: ConfigMaps first, then the Agent.
fn build(draft: &Draft, namespace: &str) -> Result<(Vec<ConfigMap>, Value), String> {
    check_name(&draft.name, "agent name")?;
    let labels = BTreeMap::from([
        (PART_OF.to_owned(), draft.name.clone()),
        (MANAGED_BY.to_owned(), MANAGER.to_owned()),
    ]);
    let config_map = |name: String, data: BTreeMap<String, String>| ConfigMap {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: Some(namespace.to_owned()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    let mut config_maps = Vec::new();
    let mut skill_refs: Vec<Value> = draft.existing_skills.iter().map(|name| json!({ "configMap": name })).collect();
    for skill in &draft.skills {
        check_name(&skill.name, "skill name")?;
        if skill.description.trim().is_empty() {
            return Err(format!("skill `{}` needs a description: the agent picks skills by it", skill.name));
        }
        let object = format!("{}-skill-{}", draft.name, skill.name);
        config_maps.push(config_map(object.clone(), BTreeMap::from([("SKILL.md".to_owned(), skill_md(skill))])));
        skill_refs.push(json!({ "configMap": object, "name": skill.name }));
    }
    let knowledge = if !draft.knowledge.is_empty() {
        let object = format!("{}-knowledge", draft.name);
        config_maps.push(config_map(object.clone(), draft.knowledge.clone()));
        Some(object)
    } else {
        draft.knowledge_ref.clone()
    };

    let mut spec = draft.spec.clone();
    // Empty form fields mean "use the default", not "set to empty".
    spec.retain(|_, value| !matches!(value, Value::Null) && value != &json!(""));
    spec.remove("skills");
    spec.remove("knowledge");
    if !skill_refs.is_empty() {
        spec.insert("skills".to_owned(), Value::Array(skill_refs));
    }
    if let Some(knowledge) = knowledge {
        spec.insert("knowledge".to_owned(), json!({ "configMap": knowledge }));
    }
    let agent = json!({
        "apiVersion": "agents.baeke.info/v1alpha1",
        "kind": "Agent",
        "metadata": { "name": draft.name, "namespace": namespace, "labels": { MANAGED_BY: MANAGER } },
        "spec": spec,
    });
    Ok((config_maps, agent))
}

fn to_yaml(config_maps: &[ConfigMap], agent: &Value) -> Result<String, serde_yaml_ng::Error> {
    let mut documents = Vec::new();
    for config_map in config_maps {
        documents.push(serde_yaml_ng::to_string(config_map)?);
    }
    documents.push(serde_yaml_ng::to_string(agent)?);
    Ok(documents.join("---\n"))
}

async fn info(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({ "context": state.context, "namespace": state.namespace, "console": state.console }))
}

/// What the form can offer: the gateway's models, its MCP routes and the
/// skill ConfigMaps already in the namespace.
async fn options(State(state): State<Arc<AppState>>) -> Response {
    let client = state.client.clone();
    let ns = &state.namespace;
    let models_api = Api::<DynamicObject>::namespaced_with(
        client.clone(),
        ns,
        &ApiResource::from_gvk(&GroupVersionKind::gvk("agentgateway.dev", "v1alpha1", "AgentgatewayModel")),
    );
    let mut models: Vec<String> = match models_api.list(&ListParams::default()).await {
        Ok(list) => list
            .items
            .iter()
            .map(|model| {
                let name = model.data.pointer("/spec/match/model").and_then(Value::as_str);
                format!("openai/{}", name.map(str::to_owned).unwrap_or_else(|| model.name_any()))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    models.sort();

    let routes_api = Api::<DynamicObject>::namespaced_with(
        client.clone(),
        ns,
        &ApiResource::from_gvk(&GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute")),
    );
    let mut mcp = Vec::new();
    if let Ok(list) = routes_api.list(&ListParams::default()).await {
        for route in list.items {
            for rule in route.data.pointer("/spec/rules").and_then(Value::as_array).into_iter().flatten() {
                let to_backend = rule
                    .get("backendRefs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .find(|backend| backend.get("kind").and_then(Value::as_str) == Some("AgentgatewayBackend"));
                let path = rule.pointer("/matches/0/path/value").and_then(Value::as_str);
                if let (Some(backend), Some(path)) = (to_backend, path) {
                    let name = backend.get("name").and_then(Value::as_str).unwrap_or("mcp");
                    mcp.push(json!({ "name": name, "url": format!("http://agentgateway.{ns}:8080{path}") }));
                }
            }
        }
    }

    let skills: Vec<Value> = match Api::<ConfigMap>::namespaced(client, ns).list(&ListParams::default()).await {
        Ok(list) => list
            .items
            .iter()
            .filter(|cm| cm.labels().get(MANAGED_BY).map(String::as_str) != Some(MANAGER))
            .filter_map(|cm| {
                let text = cm.data.as_ref()?.get("SKILL.md")?;
                let skill = parse_skill(&cm.name_any(), text);
                Some(json!({ "configMap": cm.name_any(), "description": skill.description }))
            })
            .collect(),
        Err(error) => return kube_error(error),
    };
    Json(json!({ "models": models, "mcp": mcp, "skills": skills })).into_response()
}

async fn list_agents(State(state): State<Arc<AppState>>) -> Response {
    let api = Api::<DynamicObject>::namespaced_with(state.client.clone(), &state.namespace, &agent_resource());
    match api.list(&ListParams::default()).await {
        Ok(list) => {
            let mut agents: Vec<Value> = list
                .items
                .iter()
                .map(|agent| {
                    json!({
                        "name": agent.name_any(),
                        "displayName": agent.data.pointer("/spec/name"),
                        "model": agent.data.pointer("/status/model").or(agent.data.pointer("/spec/model")),
                        "ready": agent.data.pointer("/status/ready").and_then(Value::as_bool).unwrap_or(false),
                        "message": agent.data.pointer("/status/message"),
                        "studio": agent.labels().get(MANAGED_BY).map(String::as_str) == Some(MANAGER),
                    })
                })
                .collect();
            agents.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            Json(agents).into_response()
        }
        Err(error) => kube_error(error),
    }
}

/// An existing Agent as a draft for the form. Skills and knowledge the
/// studio made come back as editable content; others stay references.
async fn get_agent(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let agents = Api::<DynamicObject>::namespaced_with(state.client.clone(), &state.namespace, &agent_resource());
    let agent = match agents.get(&name).await {
        Ok(agent) => agent,
        Err(error) => return kube_error(error),
    };
    let config_maps = Api::<ConfigMap>::namespaced(state.client.clone(), &state.namespace);
    let owned = |cm: &ConfigMap| cm.labels().get(MANAGED_BY).map(String::as_str) == Some(MANAGER);

    let mut spec = agent.data.get("spec").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut draft = Draft { name: name.clone(), ..Default::default() };
    for reference in spec.remove("skills").and_then(|skills| skills.as_array().cloned()).unwrap_or_default() {
        let Some(cm_name) = reference.get("configMap").and_then(Value::as_str) else { continue };
        match config_maps.get_opt(cm_name).await {
            Ok(Some(cm)) if owned(&cm) => {
                let folder = reference.get("name").and_then(Value::as_str).unwrap_or(cm_name);
                let text = cm.data.as_ref().and_then(|data| data.get("SKILL.md")).cloned().unwrap_or_default();
                draft.skills.push(parse_skill(folder, &text));
            }
            _ => draft.existing_skills.push(cm_name.to_owned()),
        }
    }
    if let Some(cm_name) = spec.remove("knowledge").and_then(|k| k.get("configMap").and_then(Value::as_str).map(str::to_owned)) {
        match config_maps.get_opt(&cm_name).await {
            Ok(Some(cm)) if owned(&cm) => draft.knowledge = cm.data.unwrap_or_default(),
            _ => draft.knowledge_ref = Some(cm_name),
        }
    }
    draft.spec = spec;
    Json(draft).into_response()
}

async fn preview(State(state): State<Arc<AppState>>, Json(draft): Json<Draft>) -> Response {
    match build(&draft, &state.namespace) {
        Ok((config_maps, agent)) => match to_yaml(&config_maps, &agent) {
            Ok(yaml) => Json(json!({ "yaml": yaml })).into_response(),
            Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
        },
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

/// Applies the draft: its ConfigMaps, then the Agent, then removes the
/// studio's ConfigMaps for this agent that the draft no longer uses.
async fn deploy(State(state): State<Arc<AppState>>, Json(draft): Json<Draft>) -> Response {
    let (config_maps, agent) = match build(&draft, &state.namespace) {
        Ok(objects) => objects,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let apply = PatchParams::apply(MANAGER).force().validation_strict();
    // Validate everything first, so a bad Agent does not leave ConfigMaps behind.
    let dry_run = PatchParams { dry_run: true, ..apply.clone() };
    let agents = Api::<DynamicObject>::namespaced_with(state.client.clone(), &state.namespace, &agent_resource());
    if let Err(e) = agents.patch(&draft.name, &dry_run, &Patch::Apply(&agent)).await {
        return kube_error(e);
    }
    let cms = Api::<ConfigMap>::namespaced(state.client.clone(), &state.namespace);
    let mut wanted = Vec::new();
    for config_map in &config_maps {
        let name = config_map.metadata.name.clone().unwrap_or_default();
        if let Err(e) = cms.patch(&name, &apply, &Patch::Apply(config_map)).await {
            return kube_error(e);
        }
        wanted.push(name);
    }
    if let Err(e) = agents.patch(&draft.name, &apply, &Patch::Apply(&agent)).await {
        return kube_error(e);
    }
    let selector = format!("{PART_OF}={},{MANAGED_BY}={MANAGER}", draft.name);
    if let Ok(list) = cms.list(&ListParams::default().labels(&selector)).await {
        for stale in list.items.iter().filter(|cm| !wanted.contains(&cm.name_any())) {
            let _ = cms.delete(&stale.name_any(), &DeleteParams::default()).await;
        }
    }
    tracing::info!("applied agent {}/{}", state.namespace, draft.name);
    Json(json!({ "ok": true, "console": format!("{}/#{}", state.console, draft.name) })).into_response()
}

/// Deletes the Agent (the operator's objects go with it) and the studio's ConfigMaps for it.
async fn delete_agent(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let agents = Api::<DynamicObject>::namespaced_with(state.client.clone(), &state.namespace, &agent_resource());
    if let Err(e) = agents.delete(&name, &DeleteParams::default()).await {
        return kube_error(e);
    }
    let cms = Api::<ConfigMap>::namespaced(state.client.clone(), &state.namespace);
    let selector = format!("{PART_OF}={name},{MANAGED_BY}={MANAGER}");
    if let Ok(list) = cms.list(&ListParams::default().labels(&selector)).await {
        for cm in list.items {
            let _ = cms.delete(&cm.name_any(), &DeleteParams::default()).await;
        }
    }
    Json(json!({ "ok": true })).into_response()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let env = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_owned());
    let context = env("STUDIO_CONTEXT", "kind-agents");
    let kubeconfig = KubeConfigOptions { context: Some(context.clone()), ..Default::default() };
    let config = kube::Config::from_kubeconfig(&kubeconfig).await?;
    let state = Arc::new(AppState {
        client: Client::try_from(config)?,
        namespace: env("STUDIO_NAMESPACE", "agents"),
        context,
        console: env("CONSOLE_URL", "http://localhost:32000"),
    });

    let app = Router::new()
        .route("/", get(|| async { Html(include_str!("../index.html")) }))
        .route("/api/info", get(info))
        .route("/api/options", get(options))
        .route("/api/agents", get(list_agents))
        .route("/api/agents/{name}", get(get_agent).delete(delete_agent))
        .route("/api/preview", post(preview))
        .route("/api/deploy", post(deploy))
        .with_state(state.clone());

    let addr = env("STUDIO_ADDR", "127.0.0.1:8090");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("agent studio on http://{addr} (context {}, namespace {})", state.context, state.namespace);
    axum::serve(listener, app).with_graceful_shutdown(async { tokio::signal::ctrl_c().await.ok(); }).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Draft {
        Draft {
            name: "release-writer".into(),
            spec: serde_json::from_value(json!({ "model": "openai/azure-gpt-6-luna", "instructions": "Hi", "temperature": null, "description": "" })).unwrap(),
            skills: vec![InlineSkill { name: "notes".into(), description: "Write notes: short.".into(), instructions: "1. Group.".into() }],
            existing_skills: vec!["skill-packing-list".into()],
            knowledge: BTreeMap::from([("faq.md".into(), "# FAQ".into())]),
            knowledge_ref: None,
        }
    }

    #[test]
    fn builds_configmaps_and_agent() {
        let (cms, agent) = build(&draft(), "agents").unwrap();
        let names: Vec<_> = cms.iter().map(|cm| cm.metadata.name.clone().unwrap()).collect();
        assert_eq!(names, ["release-writer-skill-notes", "release-writer-knowledge"]);
        assert_eq!(agent["spec"]["skills"][0]["configMap"], "skill-packing-list");
        assert_eq!(agent["spec"]["skills"][1]["name"], "notes");
        assert_eq!(agent["spec"]["knowledge"]["configMap"], "release-writer-knowledge");
        assert!(agent["spec"].get("temperature").is_none() && agent["spec"].get("description").is_none());
    }

    #[test]
    fn skill_round_trips_through_skill_md() {
        let skill = &draft().skills[0];
        let parsed = parse_skill("notes", &skill_md(skill));
        assert_eq!(parsed.description, "Write notes: short.");
        assert_eq!(parsed.instructions, "1. Group.");
    }

    #[test]
    fn rejects_bad_names() {
        assert!(check_name("Trip Planner", "agent name").is_err());
        assert!(check_name("trip-planner", "agent name").is_ok());
        let mut bad = draft();
        bad.skills[0].description = String::new();
        assert!(build(&bad, "agents").is_err());
    }
}
