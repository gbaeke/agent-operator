//! The `Agent` custom resource. Its spec is agent.yaml (the shared
//! `AgentConfig`, flattened) plus what only Kubernetes needs: where the
//! skills and knowledge come from, and how to run the pods.

use agent_spec::AgentConfig;
use k8s_openapi::api::core::v1::{EnvVar, ResourceRequirements};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[kube(
    group = "agents.baeke.info",
    version = "v1alpha1",
    kind = "Agent",
    namespaced,
    status = "AgentStatus",
    shortname = "ag",
    printcolumn = r#"{"name":"Model","type":"string","jsonPath":".spec.model"}"#,
    printcolumn = r#"{"name":"Ready","type":"boolean","jsonPath":".status.ready"}"#,
    printcolumn = r#"{"name":"URL","type":"string","jsonPath":".status.url","priority":1}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpec {
    /// Everything agent.yaml holds: model, instructions, tools, MCP servers, subagents, examples.
    #[serde(flatten)]
    pub agent: AgentConfig,
    /// Skills, one ConfigMap each, with a `SKILL.md` key and optional extra files.
    /// Make one with `kubectl create configmap skill-x --from-file=skills/x/`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillRef>,
    /// Markdown documents the agent can search, as the keys of one ConfigMap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge: Option<ConfigMapRef>,
    /// Pods to run. Sessions live in memory, so keep 1 until a session database is configured.
    #[serde(default = "one")]
    pub replicas: i32,
    /// The agent-runtime image. Defaults to the operator's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Extra environment variables, e.g. an API key from a Secret.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    /// CPU and memory for each pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SkillRef {
    /// The ConfigMap in the agent's namespace.
    pub config_map: String,
    /// The skill's folder name. Defaults to the ConfigMap name without a `skill-` prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl SkillRef {
    pub fn folder(&self) -> &str {
        self.name.as_deref().unwrap_or_else(|| self.config_map.strip_prefix("skill-").unwrap_or(&self.config_map))
    }
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMapRef {
    /// The ConfigMap in the agent's namespace.
    pub config_map: String,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    /// Whether every pod of the agent is ready.
    pub ready: bool,
    /// The agent's API inside the cluster, e.g. `http://trip-planner.agents:3000`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Pods ready, out of `spec.replicas`.
    pub ready_replicas: i32,
    /// The spec generation this status describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Why the agent is not ready, if it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn one() -> i32 {
    1
}
