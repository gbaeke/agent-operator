//! The agent.yaml format. The runtime reads it from an agent folder, and the
//! operator's `Agent` resource embeds the same fields in its `spec`, so the
//! two can never drift apart. The CRD schema is generated from these types
//! and their doc comments.
//!
//! Unknown keys are not rejected here, because the `Agent` spec flattens
//! these fields next to its own: the runtime reports them itself, and the
//! Kubernetes API server prunes them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5-5";

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    /// Shown in the UI. Defaults to the folder or resource name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// One sentence about the agent, shown in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `vendor/model`, e.g. `anthropic/claude-sonnet-5-5`, `openai/gpt-5.5` or
    /// `ollama/qwen3:4b`. The key comes from the vendor's usual variable.
    #[serde(default = "default_model")]
    pub model: String,
    /// Send model calls here instead, e.g. a gateway. Empty means unset.
    /// Behind a gateway the vendor's API key may be unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The most tokens one model call may produce.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,
    /// Sampling temperature. Reasoning models may reject it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Model calls per user message.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    /// The system prompt. In an agent folder it may live in `instructions.md` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The built-in tool groups to enable. Absent means all of them except
    /// `shell`, which is only on when listed. `knowledge` and `skills` only
    /// appear when there is content for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Builtin>>,
    /// MCP servers whose tools the agent and its subagents can use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServer>,
    /// Specialists the `task` tool can hand work to. Without any, `task`
    /// starts a general-purpose subagent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<SubagentConfig>,
    /// Example prompts for the UI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
}

/// A group of built-in tools.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Builtin {
    /// `write_todos`, plus a check that the plan is finished before answering.
    Planning,
    /// A scratch filesystem in the session: `ls`, `read_file`, `write_file`, `edit_file`.
    Files,
    /// Search over the knowledge documents: `search_knowledge_base`, `read_document`, `list_documents`.
    Knowledge,
    /// `load_skill` for the agent's skills.
    Skills,
    /// `task`: hand a self-contained job to a subagent with its own context.
    Subagents,
    /// `shell`: run commands with `sh` in a folder holding the session's files.
    /// Needs an image with a shell, such as `agent-runtime-python`; only on when listed.
    Shell,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    /// The server's streamable HTTP endpoint, e.g. `http://mcp-server:8081/mcp`.
    pub url: String,
    /// Only these tools of the server; absent means all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SubagentConfig {
    /// How the main agent refers to this subagent.
    pub name: String,
    /// Tells the main agent when to use this subagent.
    pub description: String,
    /// The subagent's system prompt.
    pub instructions: String,
    /// Defaults to the agent's model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Model calls per task.
    #[serde(default = "default_subagent_turns")]
    pub max_turns: usize,
}

fn default_model() -> String {
    DEFAULT_MODEL.to_owned()
}
fn default_max_tokens() -> u64 {
    8192
}
fn default_max_turns() -> usize {
    40
}
fn default_subagent_turns() -> usize {
    15
}
